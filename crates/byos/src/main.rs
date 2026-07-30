//! Binary entry point. Per ADR-0005 this stays minimal: real startup lives
//! in `run.rs` via `byos::start(std::env::args())`.

#[tokio::main]
async fn main() {
    byos::start(std::env::args()).await;
}
