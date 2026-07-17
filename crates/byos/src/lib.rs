//! BYOS service: public proposal API + CoW solver engine.
//!
//! Two listeners share an in-memory proposal store (ADR-0001):
//! - **Public** (`/proposals`): sub-solver-facing CRUD
//! - **Internal** (`/solve`): driver-facing solver engine (COW-1163)
//!
//! Internal split: `domain/` is pure logic, `infra/` owns IO (ADR-0005).

pub mod domain;
pub mod infra;
mod run;
#[cfg(test)]
mod tests;

pub use run::{run, start};
