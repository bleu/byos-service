//! Everything that talks to the outside world (ADR-0005): the HTTP API, the
//! Postgres proposal store and audit writer, chain access, the orderbook
//! client, and the background loops that drive the lifecycle.

pub mod api;
pub mod audit;
pub mod blockchain;
pub mod orderbook;
pub mod penalty;
pub mod retention;
pub mod storage;
pub mod validation;
