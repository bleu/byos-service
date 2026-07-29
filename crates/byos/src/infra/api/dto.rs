//! Edge conversions between domain types and the shared `proposal-dto` wire
//! types (ADR-0005). The wire shapes live in `proposal-dto` so the server
//! and sub-solver clients deserialize one model.

use {
    super::error::{Error, Kind},
    crate::domain,
    alloy::primitives::U256,
    proposal_dto::proposal::{Interaction, ProposalMetadata, RejectionReason, Status},
};

impl From<domain::proposal::ProposalStatus> for Status {
    fn from(status: domain::proposal::ProposalStatus) -> Self {
        use domain::proposal::ProposalStatus as S;
        match status {
            S::Submitted => Self::Submitted,
            S::Active => Self::Active,
            S::Rejected => Self::Rejected,
            S::Expired => Self::Expired,
            S::Executing => Self::Executing,
            S::Settled => Self::Settled,
            S::SettleFailed => Self::SettleFailed,
            S::Penalized => Self::Penalized,
            S::SimFailed => Self::SimFailed,
            S::Cancelled => Self::Cancelled,
        }
    }
}

impl From<domain::validator::RejectionReason> for RejectionReason {
    fn from(reason: domain::validator::RejectionReason) -> Self {
        use domain::validator::RejectionReason as R;
        match reason {
            R::InsufficientEscrow => Self::InsufficientEscrow,
            R::UnsupportedOrder => Self::UnsupportedOrder,
            R::AmountMismatch => Self::AmountMismatch,
            R::OrderNotFound => Self::OrderNotFound,
            R::Unprofitable => Self::Unprofitable,
        }
    }
}

impl From<&domain::proposal::Proposal> for ProposalMetadata {
    fn from(p: &domain::proposal::Proposal) -> Self {
        Self {
            id: p.id.0,
            sub_solver: p.sub_solver,
            valid_until: p.valid_until.to_string(),
            status: p.status.into(),
        }
    }
}

/// Converts a wire interaction into the contract type. A free function
/// because both types are foreign here, so `TryFrom` is unavailable (orphan
/// rule); parse failures map straight to a 400.
pub(crate) fn interaction(dto: &Interaction) -> Result<byos_common::contracts::Interaction, Error> {
    let value = parse_u256(&dto.value)
        .map_err(|_| Error::new(Kind::BadRequest, "invalid interaction value"))?;
    let call_data = parse_hex(&dto.call_data)
        .map_err(|_| Error::new(Kind::BadRequest, "invalid interaction callData"))?;
    Ok(byos_common::contracts::Interaction {
        target: dto.target,
        value,
        callData: call_data.into(),
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a hex string (with or without `0x` prefix) into bytes.
pub(crate) fn parse_hex(s: &str) -> Result<Vec<u8>, alloy::hex::FromHexError> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    alloy::hex::decode(s)
}

/// Parse a decimal string into `U256`.
pub(crate) fn parse_u256(s: &str) -> Result<U256, alloy::primitives::ruint::ParseError> {
    U256::from_str_radix(s, 10)
}
