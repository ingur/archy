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

use crate::{FromApp, Module, RestartPolicy, Schedule, Service, SystemResult};

// --- System Types ---

pub(crate) type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Future type for systems - returns SystemResult to enable error-based restarts
pub(crate) type SystemFuture = Pin<Box<dyn Future<Output = SystemResult> + Send>>;

/// A factory that can create system futures. Can be called multiple times (for Fixed schedules).
/// Uses Arc for zero-overhead sharing: dereference is same cost as Box, clone only on restart.
pub type SystemFactory = Arc<dyn Fn() -> SystemFuture + Send + Sync>;

/// Creates system factories from the app context
pub(crate) type SystemSpawner = Box<dyn FnOnce(&App, usize) -> Vec<SystemFactory> + Send>;

/// Configuration for a system (parallels WorkerConfig for services)
#[derive(Clone)]
pub struct SystemConfig {
    pub workers: usize,
    pub restart: RestartPolicy,
}

impl SystemConfig {
    /// Default number of workers per system.
    pub const DEFAULT_WORKERS: usize = 1;
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            workers: Self::DEFAULT_WORKERS,
            restart: RestartPolicy::default(),
        }
    }
}

/// Descriptor for a registered system
pub struct SystemDescriptor {
    pub id: TypeId,
    pub schedule: Schedule,
    pub spawner: SystemSpawner,
    pub(crate) overrides: SystemOverrides,
}

impl SystemDescriptor {
    pub(crate) fn new(id: TypeId, schedule: Schedule, spawner: SystemSpawner) -> Self {
        Self { id, schedule, spawner, overrides: SystemOverrides::default() }
    }
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
type LifecycleHook = Box<dyn FnOnce() -> BoxFuture + Send>;

/// Result of spawning a service - includes lifecycle hooks and worker futures
struct ServiceSpawnResult {
    startup: LifecycleHook,
    workers: Vec<BoxFuture>,
    shutdown: LifecycleHook,
}

type ServiceSpawner = Box<dyn FnOnce(&App, Arc<dyn Any + Send + Sync>, WorkerConfig) -> ServiceSpawnResult + Send>;

#[derive(Clone)]
pub(crate) struct WorkerConfig {
    pub(crate) workers: usize,
    pub(crate) capacity: usize,
    pub(crate) concurrent: Option<usize>,
    pub(crate) restart: RestartPolicy,
}

impl WorkerConfig {
    /// Default number of workers per service.
    pub const DEFAULT_WORKERS: usize = 1;
    /// Default channel capacity for service message queues.
    pub const DEFAULT_CAPACITY: usize = 32;
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            workers: Self::DEFAULT_WORKERS,
            capacity: Self::DEFAULT_CAPACITY,
            concurrent: None,
            restart: RestartPolicy::default(),
        }
    }
}

// --- Overrides ---

#[derive(Clone, Default)]
struct ServiceOverrides {
    workers: Option<usize>,
    capacity: Option<usize>,
    concurrent: Option<usize>,
    restart: Option<RestartPolicy>,
}

#[derive(Clone, Default)]
pub(crate) struct SystemOverrides {
    workers: Option<usize>,
    restart: Option<RestartPolicy>,
}

#[derive(Clone, Default)]
struct EventOverrides {
    capacity: Option<usize>,
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
    overrides: &'a mut ServiceOverrides,
}

impl ServiceConfigBuilder<'_> {
    pub fn workers(self, n: usize) -> Self {
        self.overrides.workers = Some(n);
        self
    }

    pub fn capacity(self, n: usize) -> Self {
        self.overrides.capacity = Some(n);
        self
    }

    // Note: Setting concurrent to 0 is equivalent to `.sequential()`
    pub fn concurrent(self, n: usize) -> Self {
        self.overrides.concurrent = Some(n);
        self
    }

    pub fn sequential(self) -> Self {
        self.overrides.concurrent = Some(0);
        self
    }

    pub fn restart(self, policy: RestartPolicy) -> Self {
        self.overrides.restart = Some(policy);
        self
    }
}

pub struct SystemConfigBuilder<'a> {
    overrides: &'a mut SystemOverrides,
}

impl SystemConfigBuilder<'_> {
    pub fn workers(self, n: usize) -> Self {
        self.overrides.workers = Some(n);
        self
    }

    pub fn restart(self, policy: RestartPolicy) -> Self {
        self.overrides.restart = Some(policy);
        self
    }
}

pub struct EventConfigBuilder<'a> {
    overrides: &'a mut EventOverrides,
}

impl EventConfigBuilder<'_> {
    pub fn capacity(self, n: usize) -> Self {
        self.overrides.capacity = Some(n);
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

// --- Supervision Helpers ---

/// Supervise service workers - restarts on panic only
async fn supervise_service<F, Fut>(name: &'static str, restart: RestartPolicy, mut factory: F)
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

/// Supervise systems - restarts on panic OR error return
async fn supervise_system<F, Fut>(name: &'static str, restart: RestartPolicy, mut factory: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = SystemResult> + Send + 'static,
{
    match restart {
        RestartPolicy::Never => {
            let result = tokio::spawn(factory()).await;
            match result {
                Ok(SystemResult::Ok) => {}
                Ok(SystemResult::Err(e)) => {
                    tracing::warn!(target: "archy", task = %name, error = %e, "system failed");
                }
                Err(e) if e.is_panic() => {
                    tracing::error!(target: "archy", task = %name, error = ?e, "system panicked");
                }
                Err(_) => {}
            }
        }
        RestartPolicy::Always => {
            loop {
                let result = tokio::spawn(factory()).await;
                match result {
                    Ok(SystemResult::Ok) => break,
                    Ok(SystemResult::Err(e)) => {
                        tracing::warn!(target: "archy", task = %name, error = %e, "system failed, restarting");
                    }
                    Err(e) if e.is_panic() => {
                        tracing::warn!(target: "archy", task = %name, error = ?e, "system panicked, restarting");
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

                let should_restart = match result {
                    Ok(SystemResult::Ok) => false,
                    Ok(SystemResult::Err(e)) => {
                        tracing::warn!(target: "archy", task = %name, error = %e, "system failed");
                        true
                    }
                    Err(e) if e.is_panic() => {
                        tracing::warn!(target: "archy", task = %name, error = ?e, "system panicked");
                        true
                    }
                    Err(_) => false,
                };

                if !should_restart {
                    break;
                }

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

                tracing::info!(target: "archy", task = %name, attempt = attempts, max_attempts = max, "restarting");
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

    service_overrides: HashMap<TypeId, ServiceOverrides>,
    event_overrides: HashMap<TypeId, EventOverrides>,

    service_defaults: ServiceOverrides,
    system_defaults: SystemOverrides,
    event_defaults: EventOverrides,

    closers: Arc<Mutex<Vec<Closer>>>,
    shutdown_hooks: Vec<LifecycleHook>,
    pub(crate) shutdown_token: CancellationToken,
    pub(crate) shutdown_timeout: Option<Duration>,
}

impl App {
    /// Default channel capacity for event buses.
    pub const DEFAULT_EVENT_CAPACITY: usize = 32;

    pub fn new() -> Self {
        Self {
            resources: HashMap::new(),
            event_buses: HashMap::new(),
            service_senders: HashMap::new(),
            event_creators: HashMap::new(),
            channel_creators: HashMap::new(),
            service_spawners: HashMap::new(),
            systems: Vec::new(),
            service_overrides: HashMap::new(),
            event_overrides: HashMap::new(),
            service_defaults: ServiceOverrides::default(),
            system_defaults: SystemOverrides::default(),
            event_defaults: EventOverrides::default(),
            closers: Arc::new(Mutex::new(Vec::new())),
            shutdown_hooks: Vec::new(),
            shutdown_token: CancellationToken::new(),
            shutdown_timeout: None,
        }
    }

    /// Configure shutdown behavior
    pub fn shutdown(&mut self) -> ShutdownConfigBuilder<'_> {
        ShutdownConfigBuilder { app: self }
    }

    /// Configure default settings for all services
    pub fn service_defaults(&mut self) -> ServiceConfigBuilder<'_> {
        ServiceConfigBuilder { overrides: &mut self.service_defaults }
    }

    /// Configure default settings for all systems
    pub fn system_defaults(&mut self) -> SystemConfigBuilder<'_> {
        SystemConfigBuilder { overrides: &mut self.system_defaults }
    }

    /// Configure default settings for all events
    pub fn event_defaults(&mut self) -> EventConfigBuilder<'_> {
        EventConfigBuilder { overrides: &mut self.event_defaults }
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

    pub fn add_event<E: Clone + Send + 'static>(&mut self) -> EventConfigBuilder<'_> {
        let id = TypeId::of::<E>();
        assert!(!self.event_creators.contains_key(&id), "Event {} already registered", std::any::type_name::<E>());
        self.event_creators.insert(id, Box::new(|capacity| {
            let (tx, _) = broadcast::channel::<E>(capacity);
            Arc::new(EventBus { sender: tx })
        }));
        EventConfigBuilder { overrides: self.event_overrides.entry(id).or_default() }
    }

    pub fn add_events<T: AddEvents>(&mut self) -> &mut Self {
        T::add_to(self);
        self
    }

    pub fn event<E: Clone + Send + 'static>(&mut self) -> EventConfigBuilder<'_> {
        let id = TypeId::of::<E>();
        assert!(self.event_creators.contains_key(&id), "Event {} not registered", std::any::type_name::<E>());
        EventConfigBuilder { overrides: self.event_overrides.entry(id).or_default() }
    }

    pub fn add_service<S: Service>(&mut self) -> ServiceConfigBuilder<'_> {
        let id = TypeId::of::<S>();
        assert!(!self.channel_creators.contains_key(&id), "Service {} already registered", std::any::type_name::<S>());

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

            // Create startup hook
            let startup_svc = service.clone();
            let startup: LifecycleHook = Box::new(move || {
                Box::pin(async move {
                    startup_svc.startup().await;
                })
            });

            // Create worker futures
            let workers: Vec<BoxFuture> = (0..config.workers).map(|_| {
                let rx = rx.as_ref().clone();
                let svc = service.clone();
                let restart = config.restart;

                match config.concurrent {
                    None => Box::pin(async move {
                        supervise_service("service worker", restart, || {
                            let rx = rx.clone();
                            let svc = svc.clone();
                            async move {
                                while let Ok(msg) = rx.recv().await {
                                    svc.clone().handle(msg).await;
                                }
                            }
                        }).await;
                    }) as BoxFuture,

                    Some(max_concurrent) => {
                        let semaphore = Arc::new(Semaphore::new(max_concurrent));
                        Box::pin(async move {
                            supervise_service("service worker", restart, || {
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
            }).collect();

            // Create shutdown hook
            let shutdown_svc = service.clone();
            let shutdown: LifecycleHook = Box::new(move || {
                Box::pin(async move {
                    shutdown_svc.shutdown().await;
                })
            });

            ServiceSpawnResult { startup, workers, shutdown }
        }));

        ServiceConfigBuilder { overrides: self.service_overrides.entry(id).or_default() }
    }

    pub fn add_services<T: AddServices>(&mut self) -> &mut Self {
        T::add_to(self);
        self
    }

    pub fn service<S: Service>(&mut self) -> ServiceConfigBuilder<'_> {
        let id = TypeId::of::<S>();
        assert!(self.channel_creators.contains_key(&id), "Service {} not registered", std::any::type_name::<S>());
        ServiceConfigBuilder { overrides: self.service_overrides.entry(id).or_default() }
    }

    /// Add a single system with the given schedule
    pub fn add_system<F, Args>(&mut self, schedule: Schedule, system: F) -> SystemConfigBuilder<'_>
    where
        F: IntoSystem<Args>,
    {
        self.systems.push(SystemDescriptor::new(
            TypeId::of::<F>(),
            schedule,
            system.into_system(),
        ));
        SystemConfigBuilder { overrides: &mut self.systems.last_mut().unwrap().overrides }
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
        let id = TypeId::of::<F>();
        let desc = self.systems.iter_mut()
            .find(|d| d.id == id)
            .expect("System not found");
        SystemConfigBuilder { overrides: &mut desc.overrides }
    }

    fn resolve_service(&self, id: TypeId) -> WorkerConfig {
        let o = self.service_overrides.get(&id);
        let d = &self.service_defaults;
        WorkerConfig {
            workers: o.and_then(|x| x.workers)
                .or(d.workers)
                .unwrap_or(WorkerConfig::DEFAULT_WORKERS),
            capacity: o.and_then(|x| x.capacity)
                .or(d.capacity)
                .unwrap_or(WorkerConfig::DEFAULT_CAPACITY),
            // concurrent(0) forces sequential mode (filters to None)
            concurrent: o.and_then(|x| x.concurrent).or(d.concurrent).filter(|&n| n > 0),
            restart: o.and_then(|x| x.restart)
                .or(d.restart)
                .unwrap_or_default(),
        }
    }

    fn resolve_system(&self, overrides: &SystemOverrides) -> SystemConfig {
        let d = &self.system_defaults;
        SystemConfig {
            workers: overrides.workers
                .or(d.workers)
                .unwrap_or(SystemConfig::DEFAULT_WORKERS),
            restart: overrides.restart
                .or(d.restart)
                .unwrap_or_default(),
        }
    }

    fn resolve_event(&self, id: TypeId) -> EventConfig {
        let o = self.event_overrides.get(&id);
        let d = &self.event_defaults;
        EventConfig {
            capacity: o.and_then(|x| x.capacity)
                .or(d.capacity)
                .unwrap_or(Self::DEFAULT_EVENT_CAPACITY),
        }
    }

    /// Run the application until shutdown
    pub async fn run(mut self) {
        let mut service_handles = Vec::new();
        let mut system_handles = Vec::new();

        // Phase 0: Create event buses
        let event_creators = std::mem::take(&mut self.event_creators);
        for (id, creator) in event_creators {
            let config = self.resolve_event(id);
            self.event_buses.insert(id, creator(config.capacity));
        }

        // Phase 1: Create service channels
        let creators = std::mem::take(&mut self.channel_creators);
        let mut receivers: HashMap<TypeId, Arc<dyn Any + Send + Sync>> = HashMap::new();
        for (id, creator) in creators {
            let config = self.resolve_service(id);
            let (tx, rx) = creator(config.capacity);
            self.service_senders.insert(id, tx);
            receivers.insert(id, rx);
        }

        // Phase 2: Run First systems (blocking)
        let first = self.extract_systems(|s| matches!(s, Schedule::First));
        self.run_phase_blocking(first).await;

        // Phase 3: Create services and run startup hooks
        let spawners = std::mem::take(&mut self.service_spawners);
        let mut startup_futures = Vec::new();
        let mut worker_batches = Vec::new();

        for (id, spawner) in spawners {
            let config = self.resolve_service(id);
            let rx = receivers.remove(&id).unwrap();
            let result = spawner(&self, rx, config);
            startup_futures.push((result.startup)());
            worker_batches.push(result.workers);
            self.shutdown_hooks.push(result.shutdown);
        }

        // Run all service startups in parallel (panic = abort)
        let startup_handles: Vec<_> = startup_futures.into_iter()
            .map(|fut| tokio::spawn(fut))
            .collect();
        for handle in startup_handles {
            handle.await.expect("service startup panicked");
        }

        // Phase 4: Spawn service workers
        for workers in worker_batches {
            for fut in workers {
                service_handles.push(tokio::spawn(fut));
            }
        }

        // Phase 5: Run Startup systems (blocking)
        let startup = self.extract_systems(|s| matches!(s, Schedule::Startup));
        self.run_phase_blocking(startup).await;

        // Phase 6: Spawn Run systems (background)
        let run = self.extract_systems(|s| matches!(s, Schedule::Run));
        for desc in run {
            let config = self.resolve_system(&desc.overrides);
            for factory in (desc.spawner)(&self, config.workers) {
                let restart = config.restart;
                system_handles.push(tokio::spawn(async move {
                    supervise_system("system", restart, || factory()).await;
                }));
            }
        }

        // Phase 7: Spawn Fixed systems (interval loops)
        let fixed = self.extract_systems(|s| matches!(s, Schedule::Fixed(_)));
        for desc in fixed {
            let duration = match desc.schedule {
                Schedule::Fixed(d) => d,
                _ => unreachable!(),
            };
            let config = self.resolve_system(&desc.overrides);
            let token = self.shutdown_token.clone();
            for factory in (desc.spawner)(&self, config.workers) {
                let token = token.clone();
                let restart = config.restart;
                system_handles.push(tokio::spawn(async move {
                    supervise_system("fixed system", restart, || {
                        let factory = factory.clone();
                        let token = token.clone();
                        async move {
                            let mut interval = tokio::time::interval(duration);
                            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                            loop {
                                tokio::select! {
                                    _ = interval.tick() => {}
                                    _ = token.cancelled() => return SystemResult::Ok,
                                }
                                match factory().await {
                                    SystemResult::Ok => {}
                                    err @ SystemResult::Err(_) => return err,
                                }
                            }
                        }
                    }).await;
                }));
            }
        }

        // Wait for shutdown
        self.shutdown_token.cancelled().await;

        // Phase 8: Run Shutdown systems (blocking)
        let shutdown = self.extract_systems(|s| matches!(s, Schedule::Shutdown));
        self.run_phase_blocking(shutdown).await;

        // Phase 9: Run service shutdown hooks (parallel, with timeout)
        let shutdown_hooks = std::mem::take(&mut self.shutdown_hooks);
        let shutdown_handles: Vec<_> = shutdown_hooks.into_iter()
            .map(|hook| tokio::spawn(hook()))
            .collect();

        let await_shutdowns = async {
            for handle in shutdown_handles {
                let _ = handle.await; // Don't panic on shutdown errors
            }
        };

        if let Some(timeout) = self.shutdown_timeout {
            if tokio::time::timeout(timeout, await_shutdowns).await.is_err() {
                tracing::warn!(target: "archy", "service shutdown hooks timed out");
            }
        } else {
            await_shutdowns.await;
        }

        // Phase 10: Close channels, abort systems, wait for services
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

        // Phase 11: Run Last systems (blocking)
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
            let config = self.resolve_system(&desc.overrides);
            for factory in (desc.spawner)(self, config.workers) {
                let restart = config.restart;
                handles.push(tokio::spawn(async move {
                    supervise_system("system", restart, || factory()).await;
                }));
            }
        }
        for h in handles { let _ = h.await; }
    }
}

impl Default for App {
    fn default() -> Self { Self::new() }
}
