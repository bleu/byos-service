//! Binary entry point. Per ADR-0005 this stays minimal: real startup
//! (arg parsing, tracing init, server bind) belongs in `run.rs` via
//! `byos::start(std::env::args())` once implemented.

fn main() {}
