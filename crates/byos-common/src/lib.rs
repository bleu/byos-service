//! Shared types, EIP-712 schema, Trampoline calldata encoding, and contract
//! ABIs for the BYOS service. This crate is the common dependency between
//! `byos` (the service), `byos-watcher` (chain watcher + escrow operator),
//! `subsolver` (reference client), and `e2e` (integration tests).
//!
//! Contract ABIs are defined via `alloy::sol!` and sourced from the
//! [`bleu/byos-contracts`](https://github.com/bleu/byos-contracts) interfaces.

pub mod contracts;
pub mod eip712;
pub mod trampoline;
