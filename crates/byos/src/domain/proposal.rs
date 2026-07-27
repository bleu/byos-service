//! Proposal domain types. The store itself is Postgres
//! ([`crate::infra::storage::ProposalStore`], ADR-0013).

use alloy::primitives::{Address, B256, Bytes, U256};

/// Server-assigned proposal identifier (newtype for type safety — a
/// `ProposalId` cannot be accidentally confused with any other `u64`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ProposalId(pub u64);

impl std::fmt::Display for ProposalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for ProposalId {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<u64>().map(Self)
    }
}

/// CoW Protocol order UID (56 bytes: 32-byte hash + 20-byte owner + 4-byte
/// validTo).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OrderUid(pub [u8; 56]);

/// `0x`-prefixed hex — the wire and evidence representation.
impl std::fmt::Display for OrderUid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&alloy::hex::encode_prefixed(self.0))
    }
}

/// Parse error for `OrderUid::from_hex`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OrderUidError {
    #[error("invalid hex: {0}")]
    Hex(#[from] alloy::hex::FromHexError),
    #[error("expected 56 bytes, got {0}")]
    Length(usize),
}

impl OrderUid {
    /// Parse a `0x`-prefixed (or bare) hex string into an `OrderUid`.
    pub fn from_hex(s: &str) -> Result<Self, OrderUidError> {
        let bytes = alloy::hex::decode(s.strip_prefix("0x").unwrap_or(s))?;
        if bytes.len() != 56 {
            return Err(OrderUidError::Length(bytes.len()));
        }
        let mut arr = [0u8; 56];
        arr.copy_from_slice(&bytes);
        Ok(Self(arr))
    }
}

impl std::str::FromStr for OrderUid {
    type Err = OrderUidError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

/// Lifecycle state of a proposal. The camelCase string form is shared by the
/// wire (serde), the `proposals.status` column (strum Display/EnumString),
/// and audit payloads — one vocabulary everywhere.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum ProposalStatus {
    /// Signature verified, awaiting background validation.
    Submitted,
    Active,
    /// Failed background gatekeeping (e.g. insufficient escrow).
    Rejected,
    Expired,
    Settled,
    SimFailed,
    Cancelled,
}

/// A stored proposal, post-validation. Domain type — never serialized directly
/// to the wire (DTOs handle that).
#[derive(Clone, Debug)]
pub struct Proposal {
    pub id: ProposalId,
    pub sub_solver: Address,
    pub order_uid: OrderUid,
    pub order_uid_hash: B256,
    pub sell_amount: U256,
    pub buy_amount: U256,
    pub sell_token: Address,
    pub buy_token: Address,
    pub interactions: Vec<byos_common::contracts::Interaction>,
    pub interactions_hash: B256,
    pub valid_until: U256,
    pub nonce: U256,
    pub signature: Bytes,
    pub status: ProposalStatus,
    /// Why the background validator rejected this proposal. Set on
    /// `Submitted → Rejected` or `Active → Rejected` (escrow re-check).
    pub rejection_reason: Option<crate::domain::validator::RejectionReason>,
    /// Gas consumed by the simulation `eth_estimateGas` call. Set by the
    /// validator on successful simulation; `None` until first validation pass.
    pub gas_used: Option<u64>,
    /// Trampoline address resolved via
    /// `TrampolineFactory.addressOf(sub_solver)`. Set by the validator on
    /// first validation; `None` until resolved.
    pub trampoline: Option<Address>,
}

/// Test fixture: a minimal proposal in the given status.
#[cfg(test)]
pub(crate) fn test_proposal(
    order_uid: OrderUid,
    sub_solver: Address,
    status: ProposalStatus,
) -> Proposal {
    let order_uid_hash = alloy::primitives::keccak256(order_uid.0);
    Proposal {
        id: ProposalId(0),
        sub_solver,
        order_uid,
        order_uid_hash,
        sell_amount: U256::from(1_000_000_u64),
        buy_amount: U256::from(990_000_u64),
        sell_token: Address::ZERO,
        buy_token: Address::ZERO,
        interactions: vec![],
        interactions_hash: B256::ZERO,
        valid_until: U256::from(u64::MAX),
        nonce: U256::from(1_u64),
        signature: Bytes::new(),
        status,
        rejection_reason: None,
        gas_used: None,
        trampoline: None,
    }
}
