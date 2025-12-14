# archy

An async application framework for Rust with services, systems, and dependency injection.

## Features

- **Services** - Actor-like components with message passing and automatic client generation
- **Systems** - Lifecycle hooks (startup, run, shutdown) with dependency injection
- **Resources** - Shared state accessible via `Res<T>`
- **Events** - Pub/sub broadcasting with `Emit<E>` and `Sub<E>`
- **Supervision** - Configurable restart policies for fault tolerance

## Installation

```toml
[dependencies]
archy = "0.1"
tokio = { version = "1", features = ["full"] }
```

## Quick Start

```rust
use archy::{App, Schedule, Shutdown};

async fn hello_world(shutdown: Shutdown) {
    println!("Hello, World!");
    shutdown.trigger();
}

#[tokio::main]
async fn main() {
    let mut app = App::new();
    app.add_system(Schedule::Run, hello_world);
    app.run().await;
}
```

## Core Concepts

### Services

Services are actor-like components that process messages. Use `#[derive(Service)]` and `#[service]` to generate message handling:

```rust
use archy::prelude::*;

#[derive(Service)]
struct CounterService {
    count: std::sync::atomic::AtomicU32,
}

#[service]
impl CounterService {
    pub async fn increment(&self) -> u32 {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }
}
```

### Systems

Systems are async functions that run at specific lifecycle phases:

- `Schedule::First` - Before services start (migrations, config validation)
- `Schedule::Startup` - After services start (cache warming)
- `Schedule::Run` - Background tasks that run until shutdown
- `Schedule::Fixed(duration)` - Periodic tasks (health checks, metrics)
- `Schedule::Shutdown` - Cleanup when shutting down
- `Schedule::Last` - Final cleanup after everything stops

### Resources

Shared state injected into services and systems:

```rust
struct Config { db_url: String }

let mut app = App::new();
app.add_resource(Config { db_url: "postgres://...".into() });

// Systems receive resources automatically
async fn my_system(config: Res<Config>) {
    println!("DB: {}", config.db_url);
}
```

### Events

Fire-and-forget pub/sub broadcasting:

```rust
#[derive(Clone)]
struct UserCreated { id: u64 }

app.add_event::<UserCreated>();

// Emit events
async fn create_user(emit: Emit<UserCreated>) {
    emit.emit(UserCreated { id: 42 });
}

// Subscribe to events
async fn log_users(mut events: Sub<UserCreated>) {
    while let Some(event) = events.recv().await {
        println!("User created: {}", event.id);
    }
}
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
