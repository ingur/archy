# archy

A lightweight async application framework for Rust. Inspired by Bevy/Axum, built for async.

## Installation

```toml
[dependencies]
archy = "0.3"
tokio = { version = "1", features = ["full"] }
```

## Quick Start

```rust
use archy::{App, Schedule, Shutdown};

async fn hello(shutdown: Shutdown) {
    println!("Hello, World!");
    shutdown.trigger();
}

#[tokio::main]
async fn main() {
    let mut app = App::new();
    app.add_system(Schedule::Run, hello);
    app.run().await;
}
```

## Services

Services are actor-like components that process messages. The `#[derive(Service)]` and `#[service]` macros generate message types and client methods:

```rust
use archy::prelude::*;

#[derive(Service)]
struct Greeter {}

#[service]
impl Greeter {
    pub async fn greet(&self, name: String) -> String {
        format!("Hello, {name}!")
    }
}
```

Register services and call them using `Client<S>`:

```rust
app.add_service::<Greeter>();
```

```rust
async fn greet_user(greeter: Client<Greeter>, shutdown: Shutdown) {
    let message = greeter.greet("World".into()).await.unwrap();
    println!("{message}");
    shutdown.trigger();
}
```

Service calls return `Result<T, ServiceError>`. Use `.timeout()` to add timeouts:

```rust
use std::time::Duration;

let result = greeter.greet("World".into()).timeout(Duration::from_secs(5)).await;
```

## Systems

Systems are async functions that run at specific lifecycle phases:

- `Schedule::First` - Before services start
- `Schedule::Startup` - After services start
- `Schedule::Run` - Background tasks until shutdown
- `Schedule::Fixed(Duration)` - Periodic interval tasks
- `Schedule::Shutdown` - When shutdown triggered
- `Schedule::Last` - After everything stops

```rust
use std::time::Duration;

app.add_system(Schedule::First, run_migrations);
app.add_system(Schedule::Startup, warmup_cache);
app.add_system(Schedule::Fixed(Duration::from_secs(30)), health_check);
```

Systems can return `Result<(), E>` to trigger restart policies on error.

## Resources

Shared state injected into services and systems:

```rust
struct Config { db_url: String }

app.add_resource(Config { db_url: "postgres://...".into() });

async fn my_system(config: Res<Config>) {
    println!("DB: {}", config.db_url);
}

// Services use the same extractors
#[derive(Service)]
struct OrderService {
    db: Res<Database>,
    payments: Client<PaymentService>,
}
```

## Events

Pub/sub broadcasting with `Emit<E>` and `Sub<E>`:

```rust
#[derive(Clone)]
struct UserCreated { id: u64 }

app.add_event::<UserCreated>();

async fn create_user(emit: Emit<UserCreated>) {
    emit.emit(UserCreated { id: 42 });
}

async fn log_users(mut events: Sub<UserCreated>) {
    while let Some(event) = events.recv().await {
        println!("User created: {}", event.id);
    }
}
```

## Configuration

Services, systems, and events can be configured:

```rust
// Service config
app.add_service::<MyService>()
    .workers(4)
    .capacity(64)
    .concurrent(10)  // or .sequential()
    .restart(RestartPolicy::Always);

// System config
app.add_system(Schedule::Run, my_system)
    .workers(2)
    .restart(RestartPolicy::attempts(3));

// Event config
app.add_event::<MyEvent>()
    .capacity(128);

// Shutdown timeout
app.shutdown().timeout(Duration::from_secs(30));
```

Set defaults before registration:

```rust
app.service_defaults().workers(2).capacity(64);
app.system_defaults().restart(RestartPolicy::Always);
app.event_defaults().capacity(128);
```

## Restart Policies

Control restart behavior for services and systems:

```rust
RestartPolicy::Never              // No restart (default)
RestartPolicy::Always             // Always restart on panic/error
RestartPolicy::Attempts { max: 3, reset_after: Some(Duration::from_secs(60)) }
RestartPolicy::attempts(3)        // Helper with default reset window
```

Services restart on panic. Systems restart on panic or `Err` return.

## Batch Registration

Register multiple items using tuples:

```rust
app.add_resources((config, db_pool, cache));
app.add_services::<(ServiceA, ServiceB, ServiceC)>();
app.add_events::<(Event1, Event2)>();
app.add_systems(Schedule::Run, (system_a, system_b, system_c));
```

## Modules

Organize registration with the `Module` trait:

```rust
struct PaymentModule { config: PaymentConfig }

impl Module for PaymentModule {
    fn register(self, app: &mut App) {
        app.add_resource(self.config);
        app.add_service::<PaymentService>();
        app.add_event::<PaymentProcessed>();
    }
}

app.add_module(PaymentModule { config });
```

## Error Handling

Service calls return `Result<T, ServiceError>`:

```rust
pub enum ServiceError {
    ChannelClosed,   // Service shutting down
    ServiceDropped,  // Service panicked
    Timeout,         // From .timeout()
}
```

Flatten nested results with `.flatten_into()`:

```rust
// When a service method returns Result<T, BusinessError>
let result: Result<User, AppError> = users.create(data).flatten_into().await;
// Requires: AppError: From<ServiceError> + From<BusinessError>
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT).
