//! The validation seam between the background loop and per-proposal judgment.
//!
//! The loop (infra) owns iteration, snapshotting, and state transitions; a
//! [`ProposalValidator`] owns only the verdict on a single proposal.

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
///
/// Returns `Some(verdict)` to transition the proposal, or `None` to skip it
/// (leave as `Submitted`, retry next tick) — used when a transient error
/// (e.g. RPC timeout) prevents judgment.
pub trait ProposalValidator: Send + Sync {
    fn validate(&self, proposal: &Proposal) -> impl Future<Output = Option<Verdict>> + Send;

    /// Called at the start of each validation tick. Implementations can use
    /// this to clear per-tick caches (e.g. escrow balance lookups).
    fn begin_tick(&self) {}
}

/// Stub validator: accepts every proposal unconditionally. Useful for tests
/// and as a fallback when no chain connectivity is needed.
pub struct AcceptAll;

impl ProposalValidator for AcceptAll {
    async fn validate(&self, _proposal: &Proposal) -> Option<Verdict> {
        Some(Verdict::Accept)
    }
}
