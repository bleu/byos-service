//! Track A penalty policy (ADR-0003): the debit amounts and the
//! chain seam the penalty loop drives. A reverted settlement costs the
//! sub-solver `gas + c_l`; winning an auction and never settling costs
//! `0.1 × c_l`. `c_l` is the configured `--min-collateral`.

use alloy::primitives::{Address, B256, U256};

/// The escrow-debit chain edge: fetching what a reverted settlement cost and
/// submitting the operator-signed `Escrow.debit` transaction. A seam like
/// [`super::validator::ValidateProposal`] — the loop's logic is tested with
/// an inline fake, the real implementor against a mocked RPC node.
pub trait DebitEscrow: Send + Sync {
    /// What the reverted settlement transaction cost on-chain:
    /// `gas_used × effective_gas_price`, from its receipt.
    fn settlement_cost(
        &self,
        tx: B256,
    ) -> impl std::future::Future<Output = Result<U256, DebitError>> + Send;

    /// Debit `amount` from the sub-solver's escrow, citing `reason` (the
    /// settlement tx hash for revert debits, ADR-0003). Resolves with the
    /// debit tx hash only once that transaction has landed.
    fn debit(
        &self,
        sub_solver: Address,
        amount: U256,
        reason: B256,
    ) -> impl std::future::Future<Output = Result<B256, DebitError>> + Send;
}

/// Chain-edge failure. Every variant means "retry next tick" — the loop
/// leaves the proposal queryable in `SettleFailed` until the debit lands
/// (ADR-0013).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DebitError {
    #[error("transient: {0}")]
    Transient(String),
}

/// Revert debit (ADR-0003): the settlement's on-chain cost plus `c_l`.
pub fn revert_debit(settlement_cost: U256, c_l: U256) -> U256 {
    settlement_cost.saturating_add(c_l)
}

/// Non-settlement debit (ADR-0003): a tenth of `c_l`.
pub fn non_settlement_debit(c_l: U256) -> U256 {
    c_l / U256::from(10)
}

/// A queued non-settlement charge (ADR-0003: won the auction, never
/// settled): everything the loop needs to debit without joining back to the
/// proposal row, which may already be swept.
#[derive(Clone, Debug)]
pub struct PendingPenalty {
    /// The `penalties` row id.
    pub id: i64,
    pub proposal_id: crate::domain::proposal::ProposalId,
    pub sub_solver: Address,
    pub order_uid: crate::domain::proposal::OrderUid,
}
