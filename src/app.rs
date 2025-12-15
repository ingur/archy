use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_channel::{Receiver, bounded};
use parking_lot::Mutex;
use tokio::sync::{broadcast, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::{FromApp, Module, RestartPolicy, Schedule, Service};

// --- System Types ---

pub(crate) type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A factory that can create system futures. Can be called multiple times (for Fixed schedules).
/// Uses Arc for zero-overhead sharing: dereference is same cost as Box, clone only on restart.
pub type SystemFactory = Arc<dyn Fn() -> BoxFuture + Send + Sync>;

/// Creates system factories from the app context
pub(crate) type SystemSpawner = Box<dyn FnOnce(&App, usize) -> Vec<SystemFactory> + Send>;

/// Configuration for a system (parallels WorkerConfig for services)
#[derive(Clone)]
pub struct SystemConfig {
    pub workers: usize,
    pub restart: RestartPolicy,
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self { workers: 1, restart: RestartPolicy::default() }
    }
}

/// Descriptor for a registered system
pub struct SystemDescriptor {
    pub id: TypeId,
    pub schedule: Schedule,
    pub spawner: SystemSpawner,
    pub config: SystemConfig,
}

// --- IntoSystem Trait ---

pub trait IntoSystem<Args>: Send + 'static {
    fn into_system(self) -> SystemSpawner;
}

// --- IntoSystemConfigs Trait ---

/// Trait for tuple system registration via add_systems
pub trait IntoSystemConfigs<Marker> {
    fn into_descriptors(self, schedule: Schedule) -> Vec<SystemDescriptor>;
}

// --- Internal Types ---

pub(crate) struct EventBus<E> {
    pub(crate) sender: broadcast::Sender<E>,
}

type Closer = Box<dyn FnOnce() + Send>;
type ChannelCreator = Box<dyn FnOnce(usize) -> (Arc<dyn Any + Send + Sync>, Arc<dyn Any + Send + Sync>) + Send>;
type EventCreator = Box<dyn FnOnce(usize) -> Arc<dyn Any + Send + Sync> + Send>;
type ServiceSpawner = Box<dyn FnOnce(&App, Arc<dyn Any + Send + Sync>, WorkerConfig) -> Vec<BoxFuture> + Send>;

#[derive(Clone)]
pub(crate) struct WorkerConfig {
    pub(crate) workers: usize,
    pub(crate) capacity: usize,
    pub(crate) concurrent: Option<usize>,
    pub(crate) restart: RestartPolicy,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self { workers: 1, capacity: 64, concurrent: None, restart: RestartPolicy::default() }
    }
}

#[derive(Clone)]
pub(crate) struct EventConfig {
    pub(crate) capacity: usize,
}

// --- Batch Registration Traits ---

pub trait AddResources {
    fn add_to(self, app: &mut App);
}

pub trait AddServices {
    fn add_to(app: &mut App);
}

pub trait AddEvents {
    fn add_to(app: &mut App);
}

// --- Config Builders ---

pub struct ServiceConfigBuilder<'a> {
    pub(crate) app: &'a mut App,
    pub(crate) id: TypeId,
}

impl ServiceConfigBuilder<'_> {
    pub fn workers(self, n: usize) -> Self {
        self.app.service_configs.get_mut(&self.id).unwrap().workers = n;
        self
    }
    pub fn capacity(self, n: usize) -> Self {
        self.app.service_configs.get_mut(&self.id).unwrap().capacity = n;
        self
    }
    /// Allow up to `n` concurrent message handlers per worker (default: sequential).
    pub fn concurrent(self, max_per_worker: usize) -> Self {
        self.app.service_configs.get_mut(&self.id).unwrap().concurrent = Some(max_per_worker);
        self
    }
    pub fn restart(self, policy: RestartPolicy) -> Self {
        self.app.service_configs.get_mut(&self.id).unwrap().restart = policy;
        self
    }
}

pub struct SystemConfigBuilder<'a> {
    pub(crate) app: &'a mut App,
    pub(crate) id: TypeId,
}

impl SystemConfigBuilder<'_> {
    fn get_config_mut(&mut self) -> &mut SystemConfig {
        &mut self.app.systems.iter_mut()
            .find(|d| d.id == self.id)
            .expect("System not found")
            .config
    }

    pub fn workers(mut self, n: usize) -> Self {
        self.get_config_mut().workers = n;
        self
    }

    pub fn restart(mut self, policy: RestartPolicy) -> Self {
        self.get_config_mut().restart = policy;
        self
    }
}

pub struct EventConfigBuilder<'a> {
    pub(crate) app: &'a mut App,
    pub(crate) id: TypeId,
}

impl EventConfigBuilder<'_> {
    pub fn capacity(self, n: usize) -> Self {
        self.app.event_configs.get_mut(&self.id).unwrap().capacity = n;
        self
    }
}

pub struct ShutdownConfigBuilder<'a> {
    pub(crate) app: &'a mut App,
}

impl ShutdownConfigBuilder<'_> {
    /// Force-abort workers if they don't drain within this duration.
    pub fn timeout(self, duration: Duration) -> Self {
        self.app.shutdown_timeout = Some(duration);
        self
    }
}

// --- Supervision Helper ---

/// Runs a task with restart policy supervision.
async fn supervise<F, Fut>(name: &str, restart: RestartPolicy, mut factory: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    match restart {
        RestartPolicy::Never => {
            let _ = tokio::spawn(factory()).await;
        }
        RestartPolicy::Always => {
            loop {
                let result = tokio::spawn(factory()).await;
                match result {
                    Ok(()) => break,
                    Err(e) if e.is_panic() => {
                        tracing::warn!(target: "archy", task = %name, error = ?e, "task panicked, restarting");
                    }
                    Err(_) => break,
                }
            }
        }
        RestartPolicy::Attempts { max, reset_after } => {
            let mut attempts = 0;
            loop {
                let start = Instant::now();
                let result = tokio::spawn(factory()).await;
                match result {
                    Ok(()) => break,
                    Err(e) if e.is_panic() => {
                        if let Some(duration) = reset_after
                            && start.elapsed() >= duration
                        {
                            attempts = 0;
                        }
                        attempts += 1;
                        if attempts >= max {
                            tracing::error!(target: "archy", task = %name, max_attempts = max, "max restart attempts reached");
                            break;
                        }
                        tracing::warn!(target: "archy", task = %name, attempt = attempts, max_attempts = max, error = ?e, "task panicked, restarting");
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

// --- App ---

pub struct App {
    pub(crate) resources: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
    pub(crate) event_buses: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
    pub(crate) service_senders: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,

    event_creators: HashMap<TypeId, EventCreator>,
    channel_creators: HashMap<TypeId, ChannelCreator>,
    service_spawners: HashMap<TypeId, ServiceSpawner>,

    pub(crate) systems: Vec<SystemDescriptor>,

    pub(crate) service_configs: HashMap<TypeId, WorkerConfig>,
    pub(crate) event_configs: HashMap<TypeId, EventConfig>,

    closers: Arc<Mutex<Vec<Closer>>>,
    pub(crate) shutdown_token: CancellationToken,
    pub(crate) shutdown_timeout: Option<Duration>,
    default_capacity: usize,
}

impl App {
    pub fn new() -> Self {
        Self {
            resources: HashMap::new(),
            event_buses: HashMap::new(),
            service_senders: HashMap::new(),
            event_creators: HashMap::new(),
            channel_creators: HashMap::new(),
            service_spawners: HashMap::new(),
            systems: Vec::new(),
            service_configs: HashMap::new(),
            event_configs: HashMap::new(),
            closers: Arc::new(Mutex::new(Vec::new())),
            shutdown_token: CancellationToken::new(),
            shutdown_timeout: None,
            default_capacity: 64,
        }
    }

    /// Configure shutdown behavior
    pub fn shutdown(&mut self) -> ShutdownConfigBuilder<'_> {
        ShutdownConfigBuilder { app: self }
    }

    /// Extract a dependency from the App (convenience helper for Service::create)
    pub fn extract<T: FromApp>(&self) -> T {
        T::from_app(self)
    }

    pub fn add_module<M: Module>(&mut self, module: M) -> &mut Self {
        module.register(self);
        self
    }

    pub fn add_resource<T: Send + Sync + 'static>(&mut self, resource: T) -> &mut Self {
        let id = TypeId::of::<T>();
        assert!(!self.resources.contains_key(&id), "Resource {} already registered", std::any::type_name::<T>());
        self.resources.insert(id, Arc::new(resource));
        self
    }

    pub fn add_resources<T: AddResources>(&mut self, resources: T) -> &mut Self {
        resources.add_to(self);
        self
    }

    pub fn add_event<E: Clone + Send + 'static>(&mut self) -> &mut Self {
        let id = TypeId::of::<E>();
        assert!(!self.event_creators.contains_key(&id), "Event {} already registered", std::any::type_name::<E>());
        self.event_configs.insert(id, EventConfig { capacity: self.default_capacity });
        self.event_creators.insert(id, Box::new(|capacity| {
            let (tx, _) = broadcast::channel::<E>(capacity);
            Arc::new(EventBus { sender: tx })
        }));
        self
    }

    pub fn add_events<T: AddEvents>(&mut self) -> &mut Self {
        T::add_to(self);
        self
    }

    pub fn event<E: Clone + Send + 'static>(&mut self) -> EventConfigBuilder<'_> {
        let id = TypeId::of::<E>();
        assert!(self.event_configs.contains_key(&id), "Event {} not registered", std::any::type_name::<E>());
        EventConfigBuilder { app: self, id }
    }

    pub fn add_service<S: Service>(&mut self) -> ServiceConfigBuilder<'_> {
        let id = TypeId::of::<S>();
        assert!(!self.service_configs.contains_key(&id), "Service {} already registered", std::any::type_name::<S>());

        self.service_configs.insert(id, WorkerConfig::default());

        let closers = self.closers.clone();
        self.channel_creators.insert(id, Box::new(move |capacity| {
            let (tx, rx) = bounded::<S::Message>(capacity);
            let closer_tx = tx.clone();
            closers.lock().push(Box::new(move || { closer_tx.close(); }));
            (Arc::new(tx), Arc::new(rx))
        }));

        self.service_spawners.insert(id, Box::new(|app, rx_any, config| {
            let rx = rx_any.downcast::<Receiver<S::Message>>().unwrap();
            let service = Arc::new(S::create(app));

            (0..config.workers).map(|_| {
                let rx = rx.as_ref().clone();
                let svc = service.clone();
                let restart = config.restart;

                match config.concurrent {
                    // Sequential mode (default): process one message at a time per worker
                    None => Box::pin(async move {
                        supervise("service worker", restart, || {
                            let rx = rx.clone();
                            let svc = svc.clone();
                            async move {
                                while let Ok(msg) = rx.recv().await {
                                    svc.clone().handle(msg).await;
                                }
                            }
                        }).await;
                    }) as BoxFuture,

                    // Concurrent mode: up to N concurrent handlers per worker
                    Some(max_concurrent) => {
                        let semaphore = Arc::new(Semaphore::new(max_concurrent));
                        Box::pin(async move {
                            supervise("service worker", restart, || {
                                let rx = rx.clone();
                                let svc = svc.clone();
                                let semaphore = semaphore.clone();
                                async move {
                                    while let Ok(msg) = rx.recv().await {
                                        let permit = semaphore.clone().acquire_owned().await
                                            .expect("semaphore closed unexpectedly");
                                        let svc = svc.clone();
                                        tokio::spawn(async move {
                                            svc.handle(msg).await;
                                            drop(permit);
                                        });
                                    }
                                }
                            }).await;
                        }) as BoxFuture
                    }
                }
            }).collect()
        }));

        ServiceConfigBuilder { app: self, id }
    }

    pub fn add_services<T: AddServices>(&mut self) -> &mut Self {
        T::add_to(self);
        self
    }

    pub fn service<S: Service>(&mut self) -> ServiceConfigBuilder<'_> {
        let id = TypeId::of::<S>();
        assert!(self.service_configs.contains_key(&id), "Service {} not registered", std::any::type_name::<S>());
        ServiceConfigBuilder { app: self, id }
    }

    /// Add a single system with the given schedule
    pub fn add_system<F, Args>(&mut self, schedule: Schedule, system: F) -> &mut Self
    where
        F: IntoSystem<Args>,
    {
        self.systems.push(SystemDescriptor {
            id: TypeId::of::<F>(),
            schedule,
            spawner: system.into_system(),
            config: SystemConfig::default(),
        });
        self
    }

    /// Add multiple systems with the given schedule: `add_systems(Schedule::Run, (sys1, sys2, sys3))`
    pub fn add_systems<M, S>(&mut self, schedule: Schedule, systems: S) -> &mut Self
    where
        S: IntoSystemConfigs<M>,
    {
        self.systems.extend(systems.into_descriptors(schedule));
        self
    }

    /// Configure a previously registered system
    pub fn system<F, Args>(&mut self, _system: F) -> SystemConfigBuilder<'_>
    where
        F: IntoSystem<Args>,
    {
        SystemConfigBuilder { app: self, id: TypeId::of::<F>() }
    }

    /// Run the application until shutdown
    pub async fn run(mut self) {
        let mut service_handles = Vec::new();
        let mut system_handles = Vec::new();

        // Phase 0: Create event buses
        let event_creators = std::mem::take(&mut self.event_creators);
        for (id, creator) in event_creators {
            let capacity = self.event_configs.get(&id).map(|c| c.capacity).unwrap_or(self.default_capacity);
            self.event_buses.insert(id, creator(capacity));
        }

        // Phase 1: Create service channels
        let creators = std::mem::take(&mut self.channel_creators);
        let mut receivers: HashMap<TypeId, Arc<dyn Any + Send + Sync>> = HashMap::new();
        for (id, creator) in creators {
            let config = self.service_configs.get(&id).cloned().unwrap_or_default();
            let (tx, rx) = creator(config.capacity);
            self.service_senders.insert(id, tx);
            receivers.insert(id, rx);
        }

        // Phase 2: Run First systems (blocking)
        let first = self.extract_systems(|s| matches!(s, Schedule::First));
        self.run_phase_blocking(first).await;

        // Phase 3: Spawn service workers
        let spawners = std::mem::take(&mut self.service_spawners);
        for (id, spawner) in spawners {
            let config = self.service_configs.get(&id).cloned().unwrap_or_default();
            let rx = receivers.remove(&id).unwrap();
            for fut in spawner(&self, rx, config) {
                service_handles.push(tokio::spawn(fut));
            }
        }

        // Phase 4: Run Startup systems (blocking)
        let startup = self.extract_systems(|s| matches!(s, Schedule::Startup));
        self.run_phase_blocking(startup).await;

        // Phase 5: Spawn Run systems (background)
        let run = self.extract_systems(|s| matches!(s, Schedule::Run));
        for desc in run {
            for factory in (desc.spawner)(&self, desc.config.workers) {
                system_handles.push(tokio::spawn(Self::supervise_system(factory, desc.config.restart)));
            }
        }

        // Phase 6: Spawn Fixed systems (interval loops)
        let fixed = self.extract_systems(|s| matches!(s, Schedule::Fixed(_)));
        for desc in fixed {
            let duration = match desc.schedule {
                Schedule::Fixed(d) => d,
                _ => unreachable!(),
            };
            let token = self.shutdown_token.clone();
            let restart = desc.config.restart;
            for factory in (desc.spawner)(&self, desc.config.workers) {
                let token = token.clone();
                system_handles.push(tokio::spawn(async move {
                    supervise("fixed system", restart, || {
                        let factory = factory.clone(); // Arc clone - warm path only
                        let token = token.clone();
                        async move {
                            let mut interval = tokio::time::interval(duration);
                            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                            loop {
                                tokio::select! {
                                    _ = interval.tick() => {}
                                    _ = token.cancelled() => break,
                                }
                                factory().await;
                            }
                        }
                    }).await;
                }));
            }
        }

        // Wait for shutdown
        self.shutdown_token.cancelled().await;

        // Phase 7: Run Shutdown systems (blocking)
        let shutdown = self.extract_systems(|s| matches!(s, Schedule::Shutdown));
        self.run_phase_blocking(shutdown).await;

        // Phase 8: Close channels, abort systems, wait for services
        let closers = std::mem::take(&mut *self.closers.lock());
        for closer in closers { closer(); }

        for handle in system_handles { handle.abort(); }

        if let Some(timeout) = self.shutdown_timeout {
            let abort_handles: Vec<_> = service_handles.iter().map(|h| h.abort_handle()).collect();
            let drain = async { for h in service_handles { let _ = h.await; } };
            if tokio::time::timeout(timeout, drain).await.is_err() {
                tracing::warn!(target: "archy", "shutdown timeout, force-aborting workers");
                for h in abort_handles { h.abort(); }
            }
        } else {
            for h in service_handles { let _ = h.await; }
        }

        // Phase 9: Run Last systems (blocking)
        let last = self.extract_systems(|s| matches!(s, Schedule::Last));
        self.run_phase_blocking(last).await;
    }

    /// Extract systems matching predicate (removes from self.systems)
    fn extract_systems(&mut self, predicate: impl Fn(&Schedule) -> bool) -> Vec<SystemDescriptor> {
        let mut extracted = Vec::new();
        let mut i = 0;
        while i < self.systems.len() {
            if predicate(&self.systems[i].schedule) {
                extracted.push(self.systems.swap_remove(i));
            } else {
                i += 1;
            }
        }
        extracted
    }

    /// Run systems and wait for completion
    async fn run_phase_blocking(&self, systems: Vec<SystemDescriptor>) {
        let mut handles = Vec::new();
        for desc in systems {
            for factory in (desc.spawner)(self, desc.config.workers) {
                handles.push(tokio::spawn(Self::supervise_system(factory, desc.config.restart)));
            }
        }
        for h in handles { let _ = h.await; }
    }

    async fn supervise_system(factory: SystemFactory, restart: RestartPolicy) {
        supervise("system", restart, || factory()).await;
    }
}

impl Default for App {
    fn default() -> Self { Self::new() }
}
