//! The ADR-0007 error body served on every non-2xx response.

use serde::{Deserialize, Serialize};

/// JSON error body: `{ "kind": "PascalCase", "description": "..." }`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Error {
    pub kind: Kind,
    pub description: String,
}

/// Machine-readable rejection kind. PascalCase on the wire; `Unknown`
/// absorbs kinds newer than the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum Kind {
    InvalidSignature,
    SignatureRecoveryFailed,
    InsufficientEscrow,
    ProposalExpired,
    /// `validUntil` is further out than `--max-proposal-lifetime` allows
    /// (ADR-0013).
    ProposalLifetimeExceeded,
    ProposalNotFound,
    ProposalNotCancellable,
    BadRequest,
    Internal,
    #[serde(other)]
    Unknown,
}
