//! The validation seam between the background loop and per-proposal judgment.
//!
//! The loop (infra) owns iteration, snapshotting, and state transitions; a
//! [`ValidateProposal`] owns only the verdict on a single proposal.

use {
    super::proposal::Proposal,
    alloy::primitives::Address,
    byos_common::hooks::Hooks,
    serde::Serialize,
};

/// Why the background validator rejected a proposal. PascalCase on the wire
/// (ADR-0007), exposed to sub-solvers via `GET /proposal/{id}`; the strum
/// derives use the same PascalCase strings for the
/// `proposals.rejection_reason` column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, strum::Display, strum::EnumString)]
#[non_exhaustive]
pub enum RejectionReason {
    InsufficientEscrow,
    /// The order is outside the simulation envelope (hooks, partial fill,
    /// non-erc20 balances — ADR-0012).
    UnsupportedOrder,
    /// The proposal's fill-or-kill amount differs from the order's (sell
    /// amount for sell orders, buy amount for buy orders).
    AmountMismatch,
    /// The orderbook does not know the proposal's order uid.
    OrderNotFound,
    /// The first simulation scored the proposal at or below the minimum
    /// (`score = surplus - gas`, ADR-0002) — it could never win an auction, so
    /// it is rejected at the gate (ADR-0013).
    Unprofitable,
}

/// Results of a successful simulation, stored on the proposal by the
/// `Accept` verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulationOutcome {
    /// Gas consumed by the simulation `eth_estimateGas` call.
    pub gas_used: u64,
    /// Trampoline resolved via `TrampolineFactory.addressOf(sub_solver)`.
    pub trampoline: Address,
    /// The order's tokens from the orderbook fetch (ADR-0012); stored on the
    /// proposal for `/solve`.
    pub sell_token: Address,
    pub buy_token: Address,
    /// Pre/post hooks from the order's `fullAppData`, stored on the proposal
    /// for simulation and `/solve` encoding.
    pub hooks: Hooks,
}

/// Outcome of validating a single proposal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Passed gatekeeping — proposal becomes `Active`. Carries the
    /// simulation outcome when a simulation ran; `None` for validators that
    /// don't simulate (escrow-only, `AcceptAll`).
    Accept(Option<SimulationOutcome>),
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
pub trait ValidateProposal: Send + Sync {
    fn validate(&self, proposal: &Proposal) -> impl Future<Output = Option<Verdict>> + Send;

    /// Called at the start of each validation tick. Implementations can use
    /// this to clear per-tick caches (e.g. escrow balance lookups).
    fn begin_tick(&self) {}
}

/// Stub validator: accepts every proposal unconditionally. Useful for tests
/// and as a fallback when no chain connectivity is needed.
pub struct AcceptAll;

impl ValidateProposal for AcceptAll {
    async fn validate(&self, _proposal: &Proposal) -> Option<Verdict> {
        Some(Verdict::Accept(None))
    }
}
