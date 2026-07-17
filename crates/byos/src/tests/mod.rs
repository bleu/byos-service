//! Service-level tests (ADR-0009): spawn the real server in-process via
//! `run()`'s bind channel and exercise the HTTP surface with signed requests.

mod cases;
mod setup;
