//! Serde wire types for the proposal API (`POST /proposals`,
//! `GET /proposals/{order_uid}`, `DELETE /proposals/{id}` — ADR-0001), shared
//! by the `byos` server and sub-solver clients so both ends deserialize one
//! model. Mirrors the `solvers-dto` pattern in cowprotocol/services
//! (ADR-0005). Conventions: camelCase JSON, 256-bit amounts as decimal
//! strings, addresses and order UIDs as hex strings.
//!
//! Not implemented yet — this crate is a skeleton.
