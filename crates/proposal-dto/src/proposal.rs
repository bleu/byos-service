//! Bodies of the proposal endpoints: submission, the owner-scoped single
//! proposal view, and per-order metadata listings.

use {
    alloy::primitives::Address,
    serde::{Deserialize, Serialize},
};

/// Body of `POST /proposals`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProposalRequest {
    /// 56-byte order UID as a hex string (with or without `0x` prefix).
    pub order_uid: String,
    /// Sell amount as a decimal string.
    pub sell_amount: String,
    /// Buy amount as a decimal string.
    pub buy_amount: String,
    /// Sub-solver's interactions.
    pub interactions: Vec<Interaction>,
    /// Unix timestamp after which the proposal expires.
    pub valid_until: String,
    /// Sub-solver-chosen nonce (no ordering or uniqueness enforcement).
    pub nonce: String,
    /// EIP-712 signature as a hex string (65 bytes).
    pub signature: String,
}

/// One sub-solver interaction: `value` as a decimal string, `call_data` as a
/// hex string.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Interaction {
    pub target: Address,
    pub value: String,
    pub call_data: String,
}

/// Body of a 202 `POST /proposals` response: the server-assigned proposal id.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProposalResponse {
    pub id: u64,
}

/// Proposal lifecycle status as served by the API. `Unknown` absorbs states
/// newer than the client, so server additions never break deserialization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    /// Signature verified, awaiting background validation.
    Submitted,
    Active,
    Rejected,
    Expired,
    /// A settlement built on this proposal is in flight (ADR-0013): frozen
    /// out of `/solve`, re-simulation, expiry, and cancellation until the
    /// driver reports an outcome or the executing timeout releases it.
    Executing,
    Settled,
    /// The settlement transaction reverted on-chain; a Track A escrow debit
    /// follows (ADR-0010).
    SettleFailed,
    /// The Track A debit landed (ADR-0003).
    Penalized,
    SimFailed,
    Cancelled,
    #[serde(other)]
    Unknown,
}

impl Status {
    /// Whether the proposal can never become executable again — the signal
    /// for a sub-solver to resubmit with a fresh nonce.
    ///
    /// `Executing` is deliberately absent: a settlement in flight can still
    /// be released back to `Active` by the executing timeout. `SettleFailed`
    /// is present even though `Penalized` may follow it, because the proposal
    /// itself is already dead at that point.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Rejected
                | Self::Expired
                | Self::Settled
                | Self::SettleFailed
                | Self::Penalized
                | Self::SimFailed
                | Self::Cancelled
        )
    }
}

/// Why the background validator rejected a proposal (PascalCase, ADR-0007).
/// `Unknown` absorbs reasons newer than the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum RejectionReason {
    InsufficientEscrow,
    UnsupportedOrder,
    AmountMismatch,
    OrderNotFound,
    /// The first simulation scored the proposal at or below the minimum, so it
    /// could never win an auction (ADR-0002, ADR-0013).
    Unprofitable,
    #[serde(other)]
    Unknown,
}

/// Body of `GET /proposals/{order_uid}` and `GET /proposals/by-sub-solver`:
/// per-proposal metadata for the caller's own proposals.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListProposalsResponse {
    pub proposals: Vec<ProposalMetadata>,
}

/// Metadata only (ADR-0001): no interactions, amounts, or signature.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposalMetadata {
    pub id: u64,
    pub sub_solver: Address,
    pub valid_until: String,
    pub status: Status,
}

/// Body of `GET /proposal/{id}`: the caller's own proposal, including the
/// async validation verdict.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetProposalResponse {
    pub id: u64,
    pub sub_solver: Address,
    pub order_uid: String,
    pub sell_amount: String,
    pub buy_amount: String,
    pub valid_until: String,
    pub status: Status,
    /// Only present when `status` is `rejected`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<RejectionReason>,
    /// Only present on settlement outcomes: the landed tx for `settled`, the
    /// reverted tx for `settleFailed` (ADR-0013).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settlement_tx_hash: Option<String>,
    /// Only present when `status` is `penalized`: the Track A escrow debit
    /// that closed the `settleFailed` story (ADR-0003).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub penalty_tx_hash: Option<String>,
}
