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
/// Holds the store, and with it an audit sender, so shutdown must abort it
/// before draining the audit writer — see `run.rs`, where skipping that
/// hangs the drain.
pub fn spawn(
    store: Arc<ProposalStore>,
    validator: impl ValidateProposal + 'static,
    period: Duration,
    executing_timeout: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // First tick a full period out — a plain `interval` fires
        // immediately, which would race service startup (and tests that
        // park the loop with a long period would still get one tick).
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        // A tick that overruns the period must not be followed by back-to-back
        // catch-up ticks: the default `Burst` would replay every missed tick
        // with no spacing, so a node that stalls and recovers gets several
        // full passes at once. Each pass still simulates each proposal only
        // once; what Burst inflates is the pass *rate*, and that is what
        // ADR-0013's per-lifetime simulation budget rests on.
        //
        // `Skip` would also stop the burst and would resume a little sooner.
        // `Delay` is the deliberate choice: a period of quiet after an
        // overrun is backoff for a node that just proved it is struggling.
        // The cost is up to one extra period before new proposals activate.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before Unix epoch")
                .as_secs();
            run_tick(&store, &validator, now, executing_timeout).await;
        }
    })
}

/// Report a transition the tick could not complete, at a level that matches
/// why.
///
/// Losing the compare-and-swap is the expected case — a cancellation or a
/// driver notification won the race — and stays at `debug`. Everything else
/// used to be logged the same way, which meant a pool timeout or an
/// unparseable row read as "stale, dropped" and vanished entirely under the
/// production `byos=info` filter.
fn note_lost_transition(
    id: &crate::domain::proposal::ProposalId,
    e: &crate::infra::storage::StoreError,
    what: &str,
) {
    use crate::infra::storage::StoreError;
    match e {
        StoreError::StaleTransition { .. } | StoreError::NotFound(_) => {
            tracing::debug!(%id, %e, "{what} lost the race, dropped");
        }
        // Everything else defers to the store's own classification rather than
        // re-deriving it here, so the two cannot drift as variants are added.
        _ if e.should_retry() => tracing::warn!(%id, %e, "{what} failed; retrying next tick"),
        // Retrying will never help: the row or the schema is the problem, and
        // it means a proposal is stuck where no sweep can move it.
        _ => tracing::error!(%id, %e, "{what} failed permanently; manual repair needed"),
    }
}

/// One pass of the background validator, in three sweeps:
///
/// 1. **Executing timeout** (ADR-0013) — any `Executing` proposal older than
///    `executing_timeout` falls back to `Active`. Runs first so a released
///    proposal joins this very tick's validation set: if its settlement
///    actually landed, re-simulation kills it now, not a tick later.
/// 2. **Expiry** — any live (`Submitted`/`Active`) proposal whose `valid_until`
///    is behind `now` flips to `Expired`. Runs before validation so an
///    already-expired submission is never validated and activated.
/// 3. **Validation** — every remaining `Submitted` and `Active` proposal is
///    judged by the validator concurrently (semaphore-bounded RPC calls) and
///    transitioned to `Active`/`Rejected`/`SimFailed`.
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
pub async fn run_tick(
    store: &ProposalStore,
    validator: &impl ValidateProposal,
    now: u64,
    executing_timeout: Duration,
) {
    validator.begin_tick();

    if let Err(e) = store.release_stale_executing(executing_timeout).await {
        // No "retrying next tick" promise here: it holds for a transient
        // failure, not for a schema fault that will fail identically forever.
        tracing::error!(%e, retryable = e.should_retry(), "executing-timeout release failed");
    }

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
            match store.transition(&proposal, ProposalStatus::Expired).await {
                Ok(()) => tracing::info!(id = %proposal.id, "proposal expired"),
                Err(e) => note_lost_transition(&proposal.id, &e, "expiry"),
            }
        } else {
            // Both Submitted and Active proposals are validated.
            to_validate.push(proposal);
        }
    }

    // A semaphore keeps at most 8 validations in flight, to avoid bursting
    // paid-RPC rate limits while still parallelizing network calls. Each
    // verdict is resolved as soon as its validation finishes.
    const MAX_CONCURRENT: usize = 8;
    let semaphore = tokio::sync::Semaphore::new(MAX_CONCURRENT);
    futures::future::join_all(to_validate.iter().map(|proposal| {
        let semaphore = &semaphore;
        async move {
            let _permit = semaphore
                .acquire()
                .await
                .expect("semaphore is never closed");
            let Some(verdict) = validator.validate(proposal).await else {
                tracing::debug!(id = %proposal.id, "validator deferred judgment, will retry next tick");
                return;
            };
            match store.resolve_verdict(proposal.id, verdict).await {
                Ok(status) => tracing::info!(id = %proposal.id, %status, "proposal validated"),
                Err(e) => note_lost_transition(&proposal.id, &e, "verdict"),
            }
        }
    }))
    .await;
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

    /// Generous enough that no test releases an Executing proposal by
    /// accident; the release tests pass their own timeout.
    const EXECUTING_TIMEOUT: Duration = Duration::from_secs(3600);

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

        let _loop = spawn(
            store.clone(),
            AcceptAll,
            Duration::from_millis(50),
            EXECUTING_TIMEOUT,
        );

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

        run_tick(&store, &AcceptAll, 0, EXECUTING_TIMEOUT).await;

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

        run_tick(&store, &FailAll, 0, EXECUTING_TIMEOUT).await;

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

        run_tick(&store, &AcceptAll, 1_001, EXECUTING_TIMEOUT).await;

        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Expired
        );
    }

    /// The tick applies the executing-timeout backstop: a stale `Executing`
    /// proposal re-enters competition (and this same tick's validation set).
    #[ignore]
    #[tokio::test]
    async fn tick_releases_executing_proposals_past_the_timeout() {
        let store = test_store().await;
        let mut proposal = submitted_proposal();
        proposal.status = ProposalStatus::Executing;
        let id = store.insert(proposal).await.expect("insert");

        run_tick(&store, &AcceptAll, 0, Duration::ZERO).await;

        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Active,
            "a zero timeout makes any Executing proposal stale"
        );
    }

    /// The executing-timeout release is not a driver-confirmed abandonment —
    /// the notification may simply be lost — so it must not queue the
    /// non-settlement charge (COW-1205); only `/notify` does.
    #[ignore]
    #[tokio::test]
    async fn timeout_release_queues_no_non_settlement_penalty() {
        let store = test_store().await;
        let mut proposal = submitted_proposal();
        proposal.status = ProposalStatus::Executing;
        store.insert(proposal).await.expect("insert");

        run_tick(&store, &AcceptAll, 0, Duration::ZERO).await;

        assert!(
            store.pending_penalties().await.expect("pending").is_empty(),
            "an unproven non-settlement must not be charged"
        );
    }

    /// Acceptance (COW-1204): while `Executing`, a proposal is neither
    /// re-simulated nor expired — its exit is a driver notification or the
    /// executing timeout, not the validation tick (ADR-0013).
    #[ignore]
    #[tokio::test]
    async fn tick_never_touches_executing_proposals() {
        let store = test_store().await;
        let mut proposal = submitted_proposal();
        proposal.status = ProposalStatus::Executing;
        // Behind the clock: the expiry sweep would flip it if it looked, and
        // FailAll would flip anything it validates.
        proposal.valid_until = U256::from(1_000_u64);
        let id = store.insert(proposal).await.expect("insert");

        run_tick(&store, &FailAll, 2_000, EXECUTING_TIMEOUT).await;

        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Executing,
            "neither the expiry sweep nor the validator may touch Executing"
        );
    }

    #[ignore]
    #[tokio::test]
    async fn tick_expires_submitted_proposals_instead_of_validating_them() {
        let store = test_store().await;
        let mut proposal = submitted_proposal();
        proposal.valid_until = U256::from(1_000_u64);
        let id = store.insert(proposal).await.expect("insert");

        run_tick(&store, &AcceptAll, 1_001, EXECUTING_TIMEOUT).await;

        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Expired
        );
    }
}
