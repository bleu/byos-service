//! Serde wire types for the BYOS proposal API, shared by the `byos` server
//! and sub-solver clients so both sides deserialize one model (ADR-0005).
//!
//! Wire conventions: camelCase JSON, 256-bit amounts as decimal strings,
//! bytes as hex strings. Every enum deserializes unrecognized values to an
//! `Unknown` variant, so server additions never break older clients.

pub mod error;
pub mod proposal;
