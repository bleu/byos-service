//! Contract ABIs sourced from
//! [`bleu/byos-contracts`](https://github.com/bleu/byos-contracts) interfaces.
//!
//! Standard CoW Protocol types (`GPv2Settlement`, `ERC20`, `GPv2TradeData`,
//! `GPv2InteractionData`) are re-exported from
//! [`cowprotocol-primitives`](https://crates.io/crates/cowprotocol-primitives).
//! Bespoke BYOS types (Trampoline, Escrow, Proposal) remain hand-written here.

use alloy::sol;

// Re-export standard CoW Protocol contract bindings so consumers don't need
// a direct `cowprotocol-primitives` dependency.
pub use cowprotocol_primitives::contracts::{
    ERC20, GPv2InteractionData, GPv2Settlement, GPv2TradeData,
};

sol! {
    /// Interaction struct mirroring `GPv2Interaction.Data`.
    ///
    /// Kept locally because `ITrampoline::execute` references it by name in
    /// the same `sol!` block. ABI-identical to
    /// [`GPv2InteractionData`](cowprotocol_primitives::contracts::GPv2InteractionData).
    struct Interaction {
        address target;
        uint256 value;
        bytes callData;
    }

    /// Fields passed to `ITrampoline::execute()` on-chain.
    ///
    /// **This is NOT the EIP-712 signing struct.** The on-chain signed struct is
    /// [`ProposalData`] (6 fields, including `interactionsHash`). The derived
    /// `SolStruct` typehash for this 5-field struct does not match
    /// `PROPOSAL_TYPEHASH` — do not use `Proposal::eip712_hash_struct()` for
    /// signing. Use `ProposalData` instead.
    struct Proposal {
        bytes32 orderUidHash;
        uint256 sellAmount;
        uint256 buyAmount;
        uint256 validUntil;
        uint256 nonce;
    }

    /// Per-sub-solver execution sandbox. Receives the trade's sell tokens from
    /// GPv2Settlement, runs the sub-solver's EIP-712-signed route, and transfers
    /// exactly the promised buy amount back to the settlement contract.
    #[sol(rpc)]
    interface ITrampoline {
        error Trampoline_OnlySettlement();
        error Trampoline_ProposalExpired();
        error Trampoline_InvalidSignature();
        error Trampoline_EthSettleBackFailed();

        function SUB_SOLVER() external view returns (address _subSolver);
        function SETTLEMENT() external view returns (address _settlement);
        function DOMAIN_SEPARATOR() external view returns (bytes32 _domainSeparator);

        function execute(
            Proposal calldata _proposal,
            Interaction[] calldata _interactions,
            address _buyToken,
            bytes calldata _signature
        ) external;
    }

    /// CREATE2 deployer for per-sub-solver Trampoline instances and the EIP-712
    /// domain anchor for proposal signatures.
    #[sol(rpc)]
    interface ITrampolineFactory {
        event TrampolineDeployed(address indexed _subSolver, address _instance);

        function SETTLEMENT() external view returns (address _settlement);
        function ensureDeployed(address _subSolver) external returns (address _instance);
        function domainSeparator() external view returns (bytes32 _domainSeparator);
        function addressOf(address _subSolver) external view returns (address _trampoline);
    }
}

sol! {
    /// Per-chain native-token ERC20 escrow holding sub-solver collateral.
    #[sol(rpc)]
    interface IEscrow {
        event Deposited(address indexed _subSolver, uint256 _amount);
        event Debited(address indexed _subSolver, uint256 _amount, bytes32 _reason);
        event Withdrawn(address indexed _subSolver, uint256 _amount);
        event Frozen(address indexed _subSolver);
        event Unfrozen(address indexed _subSolver);
        event DebitsWithdrawn(address indexed _to, uint256 _amount);
        event WithdrawalRequested(address indexed _subSolver);
        event WithdrawalCancelled(address indexed _subSolver);
        event CooldownPeriodUpdated(uint256 _oldPeriod, uint256 _newPeriod);
        event Paused(address indexed _account);
        event Unpaused(address indexed _account);

        error Escrow_InsufficientBalance();
        error Escrow_TransferFailed();
        error Escrow_NoWithdrawalRequested();
        error Escrow_CooldownNotElapsed();
        error Escrow_AccountFrozen();
        error Escrow_WithdrawalAlreadyRequested();
        error Escrow_NothingToWithdraw();
        error Escrow_EnforcedPause();
        error Escrow_ExpectedPause();
        error Escrow_WithdrawalPending();
        error Escrow_ZeroValue();
        error Escrow_NoAdmin();
        error Escrow_ZeroAddress();

        function OPERATOR_ROLE() external view returns (bytes32 _operatorRole);
        function TRAMPOLINE_FACTORY() external view returns (address _trampolineFactory);

        function withdrawalRequestedAt(address _subSolver) external view returns (uint256 _requestedAt);
        function frozen(address _subSolver) external view returns (bool _isFrozen);
        function cooldownPeriod() external view returns (uint256 _cooldownPeriod);
        function accumulatedDebits() external view returns (uint256 _accumulatedDebits);
        function paused() external view returns (bool _paused);

        function setCooldownPeriod(uint256 _period) external;
        function debit(address _subSolver, uint256 _amount, bytes32 _reason) external;
        function freeze(address _subSolver) external;
        function unfreeze(address _subSolver) external;
        function pause() external;
        function unpause() external;
        function requestWithdrawal() external;
        function executeWithdrawal() external;
        function cancelWithdrawal() external;
        function deposit(address _subSolver) external payable;
        function withdrawDebits() external;
        function effectiveBalance(address _subSolver) external view returns (uint256 _effectiveBalance);
        function withdrawableBalance() external view returns (uint256 _withdrawableBalance);

        function balanceOf(address account) external view returns (uint256);
        function transfer(address to, uint256 amount) external returns (bool);
        function transferFrom(address from, address to, uint256 amount) external returns (bool);
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
        function totalSupply() external view returns (uint256);
    }
}

sol! {
    /// EIP-712 signed proposal struct matching the on-chain `PROPOSAL_TYPEHASH`.
    ///
    /// Includes `interactionsHash` (recomputed on-chain from the interactions
    /// actually being executed). Use `ProposalData::eip712_hash_struct()` for
    /// signing — its derived typehash matches [`PROPOSAL_TYPEHASH`].
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
