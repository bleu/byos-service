//! Chain watcher and escrow operator for the BYOS service.
//!
//! The **chain watcher** polls for new blocks, detects settlement transactions,
//! attributes them to sub-solvers via Trampoline CREATE2 addresses, and
//! classifies reverts.
//!
//! The **escrow operator** consumes chain watcher output and submits on-chain
//! transactions to the Escrow contract: Track A debits on revert, Track B
//! freeze/unfreeze/debit on manual trigger.

pub mod escrow;
pub mod watcher;
