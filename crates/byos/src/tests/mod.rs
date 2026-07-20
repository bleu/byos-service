//! Service-level tests (ADR-0009): the real axum server in-process against a
//! real Postgres, driven over HTTP with signed requests. Every case is
//! `#[ignore]`d — the service refuses to boot without its audit database —
//! and runs via `just test-db` (needs the compose Postgres).

mod cases;
mod setup;
