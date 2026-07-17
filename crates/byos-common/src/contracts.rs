//! Contract ABIs for the BYOS service.
//!
//! Standard CoW Protocol types (`GPv2Settlement`, `ERC20`, `GPv2TradeData`,
//! `GPv2InteractionData`) are re-exported from
//! [`cowprotocol-primitives`](https://crates.io/crates/cowprotocol-primitives).
//!
//! Bespoke BYOS contract bindings (Trampoline, TrampolineFactory, Escrow) are
//! generated from vendored JSON ABIs produced by `forge build` in
//! [`bleu/byos-contracts`](https://github.com/bleu/byos-contracts)
//! at commit `886ee9cdc03b24b11392403a83985ddb26f5c7fa`.

use alloy::sol;

// Re-export standard CoW Protocol contract bindings so consumers don't need
// a direct `cowprotocol-primitives` dependency.
pub use cowprotocol_primitives::contracts::{
    ERC20, GPv2InteractionData, GPv2Settlement, GPv2TradeData,
};

sol!(
    #[sol(rpc)]
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

sol! {
    /// EIP-712 signed proposal struct matching the on-chain `PROPOSAL_TYPEHASH`.
    ///
    /// Includes `interactionsHash` (recomputed on-chain from the interactions
    /// actually being executed). Use `ProposalData::eip712_hash_struct()` for
    /// signing — its derived typehash matches [`PROPOSAL_TYPEHASH`].
    ///
    /// Note: the on-chain `ITrampoline::execute()` takes a 5-field `Proposal`
    /// (without `interactionsHash`) — see [`Trampoline::ITrampoline::Proposal`].
    /// Do **not** use that struct's derived EIP-712 hash for signing.
    struct ProposalData {
        bytes32 orderUidHash;
        uint256 sellAmount;
        uint256 buyAmount;
        bytes32 interactionsHash;
        uint256 validUntil;
        uint256 nonce;
    }
}

/// EIP-712 type hash for the `ProposalData` struct, matching the on-chain
/// `PROPOSAL_TYPEHASH` in the Trampoline contract (contracts ADR-0005).
///
/// ```text
/// keccak256("ProposalData(bytes32 orderUidHash,uint256 sellAmount,uint256 buyAmount,
///            bytes32 interactionsHash,uint256 validUntil,uint256 nonce)")
/// ```
pub const PROPOSAL_TYPEHASH: alloy::primitives::B256 =
    alloy::primitives::b256!("2045708f2cdb91d16aa77dec29e1d20d5d7bdc6bbbc2a4158457a9d0be739209");

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::keccak256;

    #[test]
    fn proposal_typehash_matches_type_string() {
        let computed = keccak256(
            "ProposalData(bytes32 orderUidHash,uint256 sellAmount,uint256 buyAmount,\
             bytes32 interactionsHash,uint256 validUntil,uint256 nonce)",
        );
        assert_eq!(
            computed, PROPOSAL_TYPEHASH,
            "PROPOSAL_TYPEHASH does not match keccak256 of the EIP-712 type string"
        );
    }
}
