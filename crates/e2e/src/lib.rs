//! Test-orchestration library for end-to-end tests (ADR-0009), two tiers
//! sharing one chain fixture (offline-mode's `anvil-state.json` with the BYOS
//! contracts baked in):
//!
//! - Tier 1 (`just test-e2e`): plain anvil with the preloaded state, `byos`
//!   started in-process, the reference `subsolver` driven through the full
//!   proposal → solve → settle → outcome flow, with the harness playing the
//!   driver's role.
//! - Tier 2 (`just test-e2e-full`, tests named `full_stack`): a running [`cowdao-grants/offline-mode`](https://github.com/cowdao-grants/offline-mode)
//!   stack with BYOS plugged into the real driver/autopilot as a competing
//!   solver; isolation via `evm_snapshot`/`evm_revert`.
//!
//! All tests are `#[ignore]`d by default and run single-threaded.
//!
//! Not implemented yet — this crate is a skeleton.
