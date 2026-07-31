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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_accepts_a_bare_prefix_as_empty() {
        // Not a parse failure: an interaction with no calldata is a plain value
        // transfer, and the encoder passes empty bytes through.
        assert_eq!(
            parse_hex("0x").expect("bare prefix parses"),
            Vec::<u8>::new()
        );
        assert_eq!(parse_hex("").expect("empty parses"), Vec::<u8>::new());
    }

    #[test]
    fn parse_hex_rejects_an_odd_digit_count() {
        // Half a byte is never a byte string, prefixed or not.
        assert!(parse_hex("0xabc").is_err());
        assert!(parse_hex("abc").is_err());
    }

    #[test]
    fn parse_hex_rejects_non_hex_digits() {
        assert!(parse_hex("0xzz").is_err());
        assert!(parse_hex("0x 1").is_err());
    }

    #[test]
    fn parse_u256_keeps_leading_zeros_and_rejects_other_shapes() {
        assert_eq!(
            parse_u256("000123").expect("leading zeros parse"),
            U256::from(123)
        );
        // Hex is the wrong base here — amounts are decimal on the wire
        // (ADR-0005), so `0x10` must not silently read as 16.
        assert!(parse_u256("0x10").is_err());
        assert!(parse_u256("-1").is_err());
        assert!(parse_u256("1.5").is_err());
        assert!(parse_u256(" 1").is_err());
    }

    /// Documents surprising upstream behaviour rather than endorsing it:
    /// ruint's `from_str_radix` reads an empty string as zero, so a body with
    /// `"sellAmount": ""` is parsed, not rejected. It still cannot forge a
    /// proposal — the signature covers the amounts, so a zeroed field recovers
    /// a different sub-solver, the same way any tampered field does. Worth
    /// rejecting at the edge, which changes API behaviour and so is not done
    /// here.
    #[test]
    fn parse_u256_reads_an_empty_string_as_zero() {
        assert_eq!(parse_u256("").expect("upstream accepts it"), U256::ZERO);
    }

    #[test]
    fn parse_u256_rejects_a_value_past_the_maximum() {
        assert_eq!(
            parse_u256(&U256::MAX.to_string()).expect("the maximum itself parses"),
            U256::MAX,
        );
        // 2^256, one past what U256 holds.
        let past_max =
            "115792089237316195423570985008687907853269984665640564039457584007913129639936";
        assert!(parse_u256(past_max).is_err());
    }
}
