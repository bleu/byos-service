//! Typed API error responses per ADR-0007. The wire body (`{ kind,
//! description }` and the `Kind` enum) lives in `proposal-dto`; this module
//! adds the server-side behaviour: status-code mapping and default
//! descriptions.

use axum::{Json, http::StatusCode, response::IntoResponse};
pub use proposal_dto::error::Kind;

/// A rejection to be served: the wire kind plus a description. Local wrapper
/// so `IntoResponse` can be implemented (the wire type is foreign).
#[derive(Debug)]
pub struct Error {
    pub kind: Kind,
    pub description: String,
}

impl IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        let status = match self.kind {
            Kind::InvalidSignature | Kind::SignatureRecoveryFailed => StatusCode::BAD_REQUEST,
            Kind::ProposalExpired | Kind::ProposalLifetimeExceeded | Kind::BadRequest => {
                StatusCode::BAD_REQUEST
            }
            Kind::InsufficientEscrow => StatusCode::FORBIDDEN,
            Kind::ProposalNotFound => StatusCode::NOT_FOUND,
            Kind::ProposalNotCancellable => StatusCode::CONFLICT,
            // `Unknown` exists for client-side tolerance; the server never
            // constructs it.
            Kind::Internal | Kind::Unknown => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = proposal_dto::error::Error {
            kind: self.kind,
            description: self.description,
        };
        (status, Json(body)).into_response()
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
            Kind::ProposalLifetimeExceeded => {
                "Proposal validUntil exceeds the maximum proposal lifetime"
            }
            Kind::ProposalNotFound => "Proposal not found",
            Kind::ProposalNotCancellable => "Proposal is executing or already in a terminal state",
            Kind::BadRequest => "Malformed request",
            Kind::Internal | Kind::Unknown => "Internal error",
        };
        Self {
            kind,
            description: description.to_owned(),
        }
    }
}
