//! Shared types, EIP-712 schema, Trampoline calldata encoding, and contract
//! ABIs for the BYOS service. This crate is the common dependency between
//! `byos` (the service), `subsolver` (reference client), and `e2e`
//! (integration tests). The unused `byos-watcher` placeholder also still
//! declares it.
//!
//! Contract ABIs are defined via `alloy::sol!` and sourced from the
//! [`bleu/byos-contracts`](https://github.com/bleu/byos-contracts) interfaces.

pub mod contracts;
pub mod eip712;
pub mod settlement;
pub mod trampoline;
