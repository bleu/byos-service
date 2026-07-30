//! Pure business logic (ADR-0005): the proposal lifecycle, scoring, the
//! validation envelope, penalty amounts, and audit event shapes. No IO and
//! no wall-clock reads — callers pass the time in.

pub mod audit;
pub mod order;
pub mod penalty;
pub mod proposal;
pub mod scoring;
pub mod validator;
