//! Retention sweep (ADR-0013): its own slow loop — no reason to couple it to
//! the per-block validation tick — deleting dropped-tier proposals
//! (`Rejected`/`SimFailed`/`Expired`/`Cancelled`) once they have been
//! terminal for longer than `--dropped-retention`. Consumers are polling
//! loops that observe the terminal state within one poll interval; the
//! default hour is hundreds of intervals. Money states are never swept, and
//! `audit_events` keeps the full history of swept proposals.

use {
    crate::infra::storage::ProposalStore,
    std::{sync::Arc, time::Duration},
};

/// Spawn the sweep loop: one [`ProposalStore::sweep_dropped`] every
/// `period`. Runs for the life of the process; like the validation loop it
/// holds the store (and with it an audit sender), so shutdown must abort it
/// before draining the audit writer.
pub fn spawn(
    store: Arc<ProposalStore>,
    period: Duration,
    retention: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        loop {
            interval.tick().await;
            match store.sweep_dropped(retention).await {
                Ok(0) => {}
                Ok(deleted) => tracing::info!(deleted, "retention sweep dropped proposals"),
                // Transient failures self-heal: rows just age until the next
                // pass succeeds.
                Err(e) => tracing::error!(%e, "retention sweep failed"),
            }
        }
    })
}
