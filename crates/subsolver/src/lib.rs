//! Reference sub-solver: watches the CoW orderbook for open orders, computes
//! a route, signs an EIP-712 `ProposalData` message (ADR-0001), and submits
//! it to the BYOS proposal API in a continuous polling loop. Doubles as the
//! integration-test counterpart in the `e2e` crate and as the documented
//! example for external sub-solver teams.
//!
//! Not implemented yet — this crate is a skeleton.
