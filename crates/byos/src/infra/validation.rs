//! Background validation loop (ADR-0001, async ingestion).
//!
//! `POST /proposals` only checks the signature and stores the proposal as
//! `Submitted`. Each tick of this loop validates every `Submitted` and
//! `Active` proposal via the configured [`ValidateProposal`] implementor,
//! transitioning them to `Active`/`Rejected`/`SimFailed` or updating their
//! simulation data on re-validation.

use {
    crate::{
        domain::{proposal::ProposalStatus, validator::ValidateProposal},
        infra::storage::ProposalStore,
    },
    std::{
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    },
};

/// Spawn the background validation loop: one [`run_tick`] every `period`.
///
/// The task runs for the life of the process; it is torn down with the
/// runtime on shutdown.
pub fn spawn(
    store: Arc<ProposalStore>,
    validator: impl ValidateProposal + 'static,
    period: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // First tick a full period out — a plain `interval` fires
        // immediately, which would race service startup (and tests that
        // park the loop with a long period would still get one tick).
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        loop {
            interval.tick().await;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before Unix epoch")
                .as_secs();
            run_tick(&store, &validator, now).await;
        }
    })
}

/// One pass of the background validator, in two sweeps:
///
/// 1. **Expiry** — any live (`Submitted`/`Active`) proposal whose `valid_until`
///    is behind `now` flips to `Expired`. Runs first so an already-expired
///    submission is never validated and activated.
/// 2. **Validation** — every remaining `Submitted` and `Active` proposal is
///    judged by the validator concurrently (all RPC calls in flight at once)
///    and transitioned to `Active`/`Rejected`/`SimFailed`.
///
/// `now` is a unix timestamp from the wall clock; `valid_until` is signed
/// against block timestamps. The drift is seconds at most and only affects
/// when we stop showing/simulating a proposal — the chain enforces the real
/// deadline.
///
/// Works on a single snapshot of all live proposals (one query per tick);
/// each write is a compare-and-swap transition, so a proposal cancelled
/// mid-validation keeps its cancellation (the stale verdict is dropped).
/// A database error skips the tick — the next one retries from scratch.
pub async fn run_tick(store: &ProposalStore, validator: &impl ValidateProposal, now: u64) {
    validator.begin_tick();

    let live = match store
        .snapshot_by_statuses(&[ProposalStatus::Submitted, ProposalStatus::Active])
        .await
    {
        Ok(live) => live,
        Err(e) => {
            tracing::error!(%e, "validation tick skipped: snapshot failed");
            return;
        }
    };

    let mut to_validate = Vec::new();
    for proposal in live {
        if proposal.valid_until < alloy::primitives::U256::from(now) {
            match store
                .transition(proposal.id, proposal.status, ProposalStatus::Expired)
                .await
            {
                Ok(()) => tracing::info!(id = %proposal.id, "proposal expired"),
                Err(e) => tracing::debug!(id = %proposal.id, %e, "stale expiry dropped"),
            }
        } else {
            // Both Submitted and Active proposals are validated.
            to_validate.push(proposal);
        }
    }

    // Dispatch validations in batches of 8 to avoid bursting paid-RPC rate
    // limits while still parallelizing network calls within each batch.
    const MAX_CONCURRENT: usize = 8;
    for chunk in to_validate.chunks(MAX_CONCURRENT) {
        let results = futures::future::join_all(
            chunk
                .iter()
                .map(|proposal| async move { (proposal, validator.validate(proposal).await) }),
        )
        .await;

        for (proposal, verdict) in results {
            let Some(verdict) = verdict else {
                tracing::debug!(id = %proposal.id, "validator deferred judgment, will retry next tick");
                continue;
            };
            match store.resolve_verdict(proposal.id, verdict).await {
                Ok(status) => tracing::info!(id = %proposal.id, %status, "proposal validated"),
                Err(e) => tracing::debug!(id = %proposal.id, %e, "stale verdict dropped"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            domain::{
                proposal::{OrderUid, Proposal, ProposalStatus, test_proposal},
                validator::AcceptAll,
            },
            tests::setup::TestDb,
        },
        alloy::primitives::{Address, U256},
    };

    fn submitted_proposal() -> Proposal {
        test_proposal(
            OrderUid([0xaa; 56]),
            Address::repeat_byte(0x01),
            ProposalStatus::Submitted,
        )
    }

    /// Store on a fresh database. The audit receiver is leaked to keep the
    /// channel open; these tests assert on statuses, not evidence.
    async fn test_store() -> ProposalStore {
        let db = TestDb::create().await;
        let pool = crate::infra::audit::connect_and_migrate(&db.url)
            .await
            .expect("migrations run");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::mem::forget(rx);
        ProposalStore::new(pool, tx)
    }

    #[ignore]
    #[tokio::test]
    async fn spawned_loop_validates_on_its_interval() {
        let store = std::sync::Arc::new(test_store().await);
        let id = store.insert(submitted_proposal()).await.expect("insert");

        let _loop = spawn(store.clone(), AcceptAll, Duration::from_millis(50));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let status = store.get(id).await.expect("get").expect("exists").status;
            if status == ProposalStatus::Active {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "loop never validated the proposal, still {status}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[ignore]
    #[tokio::test]
    async fn tick_flips_submitted_to_active_with_accept_all() {
        let store = test_store().await;
        let id = store.insert(submitted_proposal()).await.expect("insert");

        run_tick(&store, &AcceptAll, 0).await;

        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Active
        );
    }

    struct FailAll;

    impl crate::domain::validator::ValidateProposal for FailAll {
        async fn validate(
            &self,
            _proposal: &Proposal,
        ) -> Option<crate::domain::validator::Verdict> {
            Some(crate::domain::validator::Verdict::SimFailed)
        }
    }

    #[ignore]
    #[tokio::test]
    async fn tick_marks_sim_failed_proposals() {
        let store = test_store().await;
        let id = store.insert(submitted_proposal()).await.expect("insert");

        run_tick(&store, &FailAll, 0).await;

        let proposal = store.get(id).await.expect("get").expect("exists");
        assert_eq!(proposal.status, ProposalStatus::SimFailed);
        assert_eq!(proposal.rejection_reason, None);
    }

    #[ignore]
    #[tokio::test]
    async fn tick_expires_active_proposals_past_valid_until() {
        let store = test_store().await;
        let mut proposal = submitted_proposal();
        proposal.status = ProposalStatus::Active;
        proposal.valid_until = U256::from(1_000_u64);
        let id = store.insert(proposal).await.expect("insert");

        run_tick(&store, &AcceptAll, 1_001).await;

        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Expired
        );
    }

    #[ignore]
    #[tokio::test]
    async fn tick_expires_submitted_proposals_instead_of_validating_them() {
        let store = test_store().await;
        let mut proposal = submitted_proposal();
        proposal.valid_until = U256::from(1_000_u64);
        let id = store.insert(proposal).await.expect("insert");

        run_tick(&store, &AcceptAll, 1_001).await;

        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Expired
        );
    }
}
