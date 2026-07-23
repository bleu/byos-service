//! Wire types for the proposal API. camelCase JSON, hex strings for bytes,
//! decimal strings for 256-bit amounts (ADR-0005).

use {
    super::error::{Error, Kind},
    alloy::primitives::{Address, U256},
    serde::{Deserialize, Serialize},
};

// ---------------------------------------------------------------------------
// POST /proposals
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProposalRequest {
    /// 56-byte order UID as a hex string (with or without `0x` prefix).
    pub order_uid: String,
    /// Sell amount as a decimal string.
    pub sell_amount: String,
    /// Buy amount as a decimal string.
    pub buy_amount: String,
    /// Sell token address.
    pub sell_token: Address,
    /// Buy token address.
    pub buy_token: Address,
    /// Sub-solver's interactions.
    pub interactions: Vec<InteractionDto>,
    /// Unix timestamp after which the proposal expires.
    pub valid_until: String,
    /// Sub-solver-chosen nonce (no ordering or uniqueness enforcement).
    pub nonce: String,
    /// EIP-712 signature as a hex string (65 bytes).
    pub signature: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractionDto {
    pub target: Address,
    pub value: String,
    pub call_data: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProposalResponse {
    pub id: crate::domain::proposal::ProposalId,
}

// ---------------------------------------------------------------------------
// GET /proposals/{order_uid}
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListProposalsResponse {
    pub proposals: Vec<ProposalMetadata>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposalMetadata {
    pub id: crate::domain::proposal::ProposalId,
    pub sub_solver: Address,
    pub valid_until: String,
    pub status: String,
}

// ---------------------------------------------------------------------------
// GET /proposals/{id} (single)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetProposalResponse {
    pub id: crate::domain::proposal::ProposalId,
    pub sub_solver: Address,
    pub order_uid: String,
    pub sell_amount: String,
    pub buy_amount: String,
    pub valid_until: String,
    pub status: String,
    /// Only present when `status` is `rejected`. PascalCase enum (ADR-0007).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<crate::domain::validator::RejectionReason>,
}

// ---------------------------------------------------------------------------
// Trait impls
// ---------------------------------------------------------------------------

impl From<&crate::domain::proposal::Proposal> for ProposalMetadata {
    fn from(p: &crate::domain::proposal::Proposal) -> Self {
        Self {
            id: p.id,
            sub_solver: p.sub_solver,
            valid_until: p.valid_until.to_string(),
            status: p.status.to_string(),
        }
    }
}

impl TryFrom<&InteractionDto> for byos_common::contracts::Interaction {
    type Error = Error;

    fn try_from(dto: &InteractionDto) -> Result<Self, Self::Error> {
        let value = parse_u256(&dto.value)
            .map_err(|_| Error::new(Kind::BadRequest, "invalid interaction value"))?;
        let call_data = parse_hex(&dto.call_data)
            .map_err(|_| Error::new(Kind::BadRequest, "invalid interaction callData"))?;
        Ok(Self {
            target: dto.target,
            value,
            callData: call_data.into(),
        })
    }
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
