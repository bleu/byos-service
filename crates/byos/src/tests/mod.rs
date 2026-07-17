//! Service-level tests (ADR-0009): the real axum server in-process against a
//! real Postgres. DB-backed cases are `#[ignore]`d and name-prefixed
//! `audit_db_`; run them with `just test-db` (needs the compose Postgres).

mod cases;
mod setup;
