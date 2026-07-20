//! Typed API error types per ADR-0007.

use {
    axum::{Json, http::StatusCode, response::IntoResponse},
    serde::Serialize,
};

/// Machine-readable rejection kind. PascalCase on the wire.
#[derive(Debug, Clone, Copy, Serialize)]
pub enum Kind {
    InvalidSignature,
    SignatureRecoveryFailed,
    InsufficientEscrow,
    ProposalExpired,
    ProposalNotFound,
    NotProposalOwner,
    ProposalNotCancellable,
    BadRequest,
    Internal,
}

/// JSON error body: `{ "kind": "PascalCase", "description": "..." }`.
#[derive(Debug, Serialize)]
pub struct Error {
    pub kind: Kind,
    pub description: String,
}

impl IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        let status = match self.kind {
            Kind::InvalidSignature | Kind::SignatureRecoveryFailed => StatusCode::BAD_REQUEST,
            Kind::ProposalExpired | Kind::BadRequest => StatusCode::BAD_REQUEST,
            Kind::InsufficientEscrow | Kind::NotProposalOwner => StatusCode::FORBIDDEN,
            Kind::ProposalNotFound => StatusCode::NOT_FOUND,
            Kind::ProposalNotCancellable => StatusCode::CONFLICT,
            Kind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(self)).into_response()
    }
}

impl Error {
    pub fn new(kind: Kind, description: impl Into<String>) -> Self {
        Self {
            kind,
            description: description.into(),
        }
    }
}

impl From<Kind> for Error {
    fn from(kind: Kind) -> Self {
        let description = match kind {
            Kind::InvalidSignature => "Invalid EIP-712 signature",
            Kind::SignatureRecoveryFailed => "Could not recover signer from signature",
            Kind::InsufficientEscrow => "Sub-solver escrow balance below minimum",
            Kind::ProposalExpired => "Proposal validUntil is in the past",
            Kind::ProposalNotFound => "Proposal not found",
            Kind::NotProposalOwner => "Signer does not match proposal sub-solver",
            Kind::ProposalNotCancellable => "Proposal is already in a terminal state",
            Kind::BadRequest => "Malformed request",
            Kind::Internal => "Internal error",
        };
        Self {
            kind,
            description: description.to_owned(),
        }
    }
}
