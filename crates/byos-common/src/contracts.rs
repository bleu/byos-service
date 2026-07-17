//! Contract ABIs sourced from
//! [`bleu/byos-contracts`](https://github.com/bleu/byos-contracts) interfaces.
//!
//! These `sol!` definitions generate Rust types matching the on-chain ABI,
//! used for calldata encoding, event parsing, and EIP-712 struct hashing.

use alloy::sol;

sol! {
    /// Minimal ERC-20 interface for token transfers during settlement encoding.
    #[sol(rpc)]
    interface IERC20 {
        function transfer(address to, uint256 amount) external returns (bool);
        function transferFrom(address from, address to, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
    }
}

sol! {
    /// Shared interaction struct mirroring `GPv2Interaction.Data`, used by both
    /// the Trampoline and settlement interfaces.
    #[derive(Debug)]
    struct Interaction {
        address target;
        uint256 value;
        bytes callData;
    }

    /// Signed proposal fields (ADR-0005), minus `interactionsHash` which is
    /// recomputed on-chain from the interactions actually being executed.
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

    /// Mirrors GPv2Trade.Data — the trade struct passed to `settle()`.
    struct GPv2TradeData {
        uint256 sellTokenIndex;
        uint256 buyTokenIndex;
        address receiver;
        uint256 sellAmount;
        uint256 buyAmount;
        uint32 validTo;
        bytes32 appData;
        uint256 feeAmount;
        uint256 flags;
        uint256 executedAmount;
        bytes signature;
    }

    /// Minimal interface of the CoW Protocol settlement contract.
    #[sol(rpc)]
    interface IGPv2Settlement {
        function settle(
            address[] calldata _tokens,
            uint256[] calldata _clearingPrices,
            GPv2TradeData[] calldata _trades,
            Interaction[][3] calldata _interactions
        ) external;

        function domainSeparator() external view returns (bytes32 _domainSeparator);
        function vaultRelayer() external view returns (address _vaultRelayer);
        function authenticator() external view returns (address _authenticator);
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
