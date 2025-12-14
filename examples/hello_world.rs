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
