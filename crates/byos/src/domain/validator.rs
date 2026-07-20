//! The validation seam between the background loop and per-proposal judgment.
//!
//! The loop (infra) owns iteration, snapshotting, and state transitions; a
//! [`ProposalValidator`] owns only the verdict on a single proposal. COW-1162
//! supplies the real implementation (escrow balance read + simulation
//! `eth_call`); until then [`AcceptAll`] stands in.

use {super::proposal::Proposal, serde::Serialize};

/// Why the background validator rejected a proposal. PascalCase on the wire
/// (ADR-0007), exposed to sub-solvers via `GET /proposal/{id}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub enum RejectionReason {
    InsufficientEscrow,
}

/// Outcome of validating a single proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Passed gatekeeping — proposal becomes `Active`.
    Accept,
    /// Failed a gatekeeping rule (e.g. escrow) — proposal becomes `Rejected`.
    Reject(RejectionReason),
    /// Simulation reverted — proposal becomes `SimFailed`.
    SimFailed,
}

/// Judges a single proposal. Async because real implementations do RPC.
pub trait ProposalValidator: Send + Sync {
    fn validate(&self, proposal: &Proposal) -> impl Future<Output = Verdict> + Send;
}

/// M1 stub: accepts every proposal unconditionally, mirroring the previous
/// inline placeholder. Replaced by the real escrow + simulation validator in
/// COW-1162.
pub struct AcceptAll;

impl ProposalValidator for AcceptAll {
    async fn validate(&self, _proposal: &Proposal) -> Verdict {
        Verdict::Accept
    }
}
