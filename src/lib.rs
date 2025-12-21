use std::any::TypeId;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_channel::Sender;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

// --- Module & Re-exports ---

mod app;
mod tuples;

pub use app::{
    App,
    // Config builders
    ServiceConfigBuilder, SystemConfigBuilder, EventConfigBuilder, ShutdownConfigBuilder,
    // System traits (needed for add_system to work)
    IntoSystem, IntoSystemConfigs, SystemDescriptor, SystemFactory,
    // Batch traits
    AddResources, AddServices, AddEvents,
};

// Re-export for macro-generated code
pub use tokio;
pub use async_channel;
pub use archy_macros::{Service, service};

// --- Prelude ---

pub mod prelude {
    pub use crate::{
        App, Res, Client, Emit, Sub, Shutdown,
        Service, ServiceFactory, ClientMethods, ServiceFutureExt, ServiceResultExt, FromApp, Module,
        Schedule, RestartPolicy, ServiceError, SystemOutput,
        service,
    };
}

// --- Core Types & Errors ---

/// Error returned when a service call fails
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceError {
    /// The service channel was closed (app shutting down)
    ChannelClosed,
    /// The service dropped before responding (likely panicked)
    ServiceDropped,
    /// The operation timed out
    Timeout,
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceError::ChannelClosed => write!(f, "service channel closed"),
            ServiceError::ServiceDropped => write!(f, "service dropped before responding"),
            ServiceError::Timeout => write!(f, "operation timed out"),
        }
    }
}

impl std::error::Error for ServiceError {}

// --- Extension Traits ---

/// Adds `.timeout(duration)` to service calls.
pub trait ServiceFutureExt<T>: Future<Output = Result<T, ServiceError>> + Sized {
    fn timeout(self, duration: Duration) -> impl Future<Output = Result<T, ServiceError>> + Send
    where
        Self: Send,
    {
        async move {
            match tokio::time::timeout(duration, self).await {
                Ok(result) => result,
                Err(_) => Err(ServiceError::Timeout),
            }
        }
    }
}

impl<F, T> ServiceFutureExt<T> for F where F: Future<Output = Result<T, ServiceError>> + Sized {}

/// Adds `.flatten_into::<AppError>()` to flatten `Result<Result<T,E>, ServiceError>` into `Result<T, AppError>`.
/// Requires `AppError: From<ServiceError> + From<E>`.
pub trait ServiceResultExt<T, E>: Future<Output = Result<Result<T, E>, ServiceError>> + Sized {
    fn flatten_into<U>(self) -> impl Future<Output = Result<T, U>> + Send
    where
        Self: Send,
        U: From<ServiceError> + From<E>,
    {
        async move {
            match self.await {
                Ok(Ok(val)) => Ok(val),
                Ok(Err(e)) => Err(U::from(e)),
                Err(e) => Err(U::from(e)),
            }
        }
    }
}

impl<F, T, E> ServiceResultExt<T, E> for F where F: Future<Output = Result<Result<T, E>, ServiceError>> + Sized {}

// --- Schedule & RestartPolicy ---

/// Schedule determines when a system runs during the app lifecycle
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Schedule {
    /// Runs before service workers spawn, blocks until complete
    First,
    /// Runs after service workers spawn, blocks until complete
    Startup,
    /// Spawns as background task, runs until shutdown
    Run,
    /// Spawns as background task, runs repeatedly on interval
    Fixed(Duration),
    /// Runs when shutdown triggered, blocks until complete
    Shutdown,
    /// Runs after everything stopped, blocks until complete
    Last,
}

/// Restart policy for service workers and systems when they panic
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartPolicy {
    /// Worker exits on panic, no restart. (Default)
    #[default]
    Never,
    /// Always restart the worker immediately.
    Always,
    /// Restart up to `max` times.
    /// If `reset_after` is Some(duration) and the worker survives that long without panicking,
    /// the attempt counter resets to 0.
    Attempts {
        max: usize,
        reset_after: Option<Duration>,
    },
}

impl RestartPolicy {
    // Default stabilization window for attempt-based restart policy.
    pub const DEFAULT_RESET_AFTER: Duration = Duration::from_secs(60);

    /// Helper: Restart up to N times with default stabilization window.
    pub fn attempts(n: usize) -> Self {
        Self::Attempts {
            max: n,
            reset_after: Some(Self::DEFAULT_RESET_AFTER),
        }
    }
}

// --- System Result & Output ---

/// Result of system execution
#[derive(Debug)]
pub enum SystemResult {
    /// System completed successfully
    Ok,
    /// System failed with an error (triggers restart policy)
    Err(String),
}

/// Trait for types that can be returned from system functions.
///
/// Implemented for `()` (always succeeds) and `Result<(), E>` (error triggers restart).
pub trait SystemOutput: Send {
    fn into_result(self) -> SystemResult;
}

impl SystemOutput for () {
    #[inline(always)]
    fn into_result(self) -> SystemResult {
        SystemResult::Ok
    }
}

impl<E: std::fmt::Display + Send + 'static> SystemOutput for Result<(), E> {
    #[inline]
    fn into_result(self) -> SystemResult {
        match self {
            Ok(()) => SystemResult::Ok,
            Err(e) => SystemResult::Err(e.to_string()),
        }
    }
}

// --- Core Wrapper Types ---

pub struct Res<T>(pub Arc<T>);

impl<T> Clone for Res<T> {
    fn clone(&self) -> Self { Res(self.0.clone()) }
}

impl<T> std::ops::Deref for Res<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target { &self.0 }
}

pub struct Client<S: Service> {
    methods: S::ClientMethods,
}

impl<S: Service> Clone for Client<S> {
    fn clone(&self) -> Self {
        Client { methods: self.methods.clone() }
    }
}

impl<S: Service> std::ops::Deref for Client<S> {
    type Target = S::ClientMethods;
    fn deref(&self) -> &Self::Target {
        &self.methods
    }
}

/// Event emitter - fire-and-forget broadcast to all subscribers.
#[derive(Clone)]
pub struct Emit<E> {
    sender: broadcast::Sender<E>,
}

impl<E: Clone + Send> Emit<E> {
    /// Emit an event to all subscribers. Returns receiver count.
    pub fn emit(&self, event: E) -> usize {
        self.sender.send(event).unwrap_or(0)
    }
}

/// Event subscriber - receives events, skips missed if lagging.
pub struct Sub<E> {
    receiver: broadcast::Receiver<E>,
}

impl<E: Clone> Clone for Sub<E> {
    fn clone(&self) -> Self {
        Sub { receiver: self.receiver.resubscribe() }
    }
}

impl<E: Clone> Sub<E> {
    pub async fn recv(&mut self) -> Option<E> {
        loop {
            match self.receiver.recv().await {
                Ok(event) => return Some(event),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(target: "archy", skipped = n, "event subscriber lagged");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

#[derive(Clone)]
pub struct Shutdown(CancellationToken);

impl Shutdown {
    /// Trigger application shutdown.
    pub fn trigger(&self) { self.0.cancel(); }

    /// Returns a future that completes when shutdown is triggered.
    ///
    /// Useful in `select!` patterns:
    /// ```ignore
    /// tokio::select! {
    ///     msg = rx.recv() => { /* handle */ }
    ///     _ = shutdown.cancelled() => { break; }
    /// }
    /// ```
    pub fn cancelled(&self) -> impl Future<Output = ()> + Send + '_ {
        self.0.cancelled()
    }
}

// --- Traits ---

/// Trait for generated client methods structs
pub trait ClientMethods<S: Service>: Clone + Send + Sync + 'static {
    fn from_sender(sender: Sender<S::Message>) -> Self;
}

pub trait Service: Sized + Send + Sync + 'static {
    type Message: Send + 'static;
    type ClientMethods: ClientMethods<Self>;

    fn create(app: &App) -> Self;
    fn handle(self: Arc<Self>, msg: Self::Message) -> impl Future<Output = ()> + Send;
}

/// Helper trait generated by #[derive(Service)] - provides dependency injection
pub trait ServiceFactory: Sized + Send + Sync + 'static {
    fn create(app: &App) -> Self;
}

pub trait FromApp: Clone + Send + Sync + 'static {
    fn from_app(app: &App) -> Self;
}

pub trait Module {
    fn register(self, app: &mut App);
}

// --- FromApp Implementations ---

impl<T: Send + Sync + 'static> FromApp for Res<T> {
    fn from_app(app: &App) -> Self {
        let id = TypeId::of::<T>();
        let res = app.resources.get(&id)
            .unwrap_or_else(|| panic!("Resource {} not registered", std::any::type_name::<T>()));
        Res(res.clone().downcast::<T>().unwrap())
    }
}

impl<S: Service> FromApp for Client<S> {
    fn from_app(app: &App) -> Self {
        let id = TypeId::of::<S>();
        let sender = app.service_senders.get(&id)
            .unwrap_or_else(|| panic!("Service {} not registered", std::any::type_name::<S>()));
        let sender_ref = (**sender).downcast_ref::<Sender<S::Message>>()
            .expect("Service sender type mismatch");
        Client {
            methods: S::ClientMethods::from_sender(sender_ref.clone())
        }
    }
}

impl<E: Clone + Send + 'static> FromApp for Emit<E> {
    fn from_app(app: &App) -> Self {
        let id = TypeId::of::<E>();
        let bus = app.event_buses.get(&id)
            .unwrap_or_else(|| panic!("Event {} not registered", std::any::type_name::<E>()));
        let bus_ref = (**bus).downcast_ref::<app::EventBus<E>>()
            .expect("Event bus type mismatch");
        Emit { sender: bus_ref.sender.clone() }
    }
}

impl<E: Clone + Send + 'static> FromApp for Sub<E> {
    fn from_app(app: &App) -> Self {
        let id = TypeId::of::<E>();
        let bus = app.event_buses.get(&id)
            .unwrap_or_else(|| panic!("Event {} not registered", std::any::type_name::<E>()));
        let bus_ref = (**bus).downcast_ref::<app::EventBus<E>>()
            .expect("Event bus type mismatch");
        Sub { receiver: bus_ref.sender.subscribe() }
    }
}

impl FromApp for Shutdown {
    fn from_app(app: &App) -> Self {
        Shutdown(app.shutdown_token.clone())
    }
}

impl FromApp for () {
    fn from_app(_app: &App) -> Self { }
}
