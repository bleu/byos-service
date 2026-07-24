//! Binary entry point. Per ADR-0005 this stays minimal: real startup lives
//! in `run.rs` via `subsolver::start(std::env::args())`.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    subsolver::start(std::env::args()).await
}
