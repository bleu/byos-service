//! Contract ABIs for the BYOS service.
//!
//! Standard CoW Protocol types (`GPv2Settlement`, `ERC20`, `GPv2TradeData`,
//! `GPv2InteractionData`) are re-exported from
//! [`cowprotocol-primitives`](https://crates.io/crates/cowprotocol-primitives).
//!
//! Bespoke BYOS contract bindings (Trampoline, TrampolineFactory, Escrow) are
//! generated from the vendored JSON ABIs in `abis/`. Those are not edited by
//! hand: `just sync-abis` regenerates them from the `byos-contracts`
//! submodule, which is the pin of record for which contracts this service
//! encodes calls for (see
//! [ADR-0014](../../../docs/adr/0014-contract-artifact-provenance.md)).

use alloy::sol;
// Re-export standard CoW Protocol contract bindings so consumers don't need
// a direct `cowprotocol-primitives` dependency.
pub use cowprotocol_primitives::contracts::{
    ERC20,
    GPv2InteractionData,
    GPv2Settlement,
    GPv2TradeData,
};

sol!(
    #[sol(rpc, all_derives)]
    Trampoline,
    "abis/Trampoline.json"
);

sol!(
    #[sol(rpc)]
    TrampolineFactory,
    "abis/TrampolineFactory.json"
);

sol!(
    #[sol(rpc)]
    Escrow,
    "abis/Escrow.json"
);

sol!(
    #[sol(rpc)]
    HooksTrampoline,
    "abis/HooksTrampoline.json"
);

// Re-export the Proposal and Interaction structs from the Trampoline ABI at the
// module level for ergonomic access (used by eip712, subsolver, tests, etc.).
pub use ITrampoline::{Interaction, Proposal};
