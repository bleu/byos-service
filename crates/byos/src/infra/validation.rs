//! Background validation loop (ADR-0001, async ingestion).
//!
//! `POST /proposals` only checks the signature and stores the proposal as
//! `Submitted`. Each tick of this loop judges every `Submitted` proposal via
//! the configured [`ProposalValidator`] and transitions it to
//! `Active`/`Rejected`/`SimFailed`.

use {
    crate::domain::{
        proposal::{InMemoryProposalStore, ProposalStatus},
        validator::ProposalValidator,
    },
    std::{
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    },
};

/// Spawn the background validation loop: one [`run_tick`] every `period`.
///
/// The task runs for the life of the process; it is torn down with the
/// runtime on shutdown. A tick does bounded in-memory work today — when the
/// validator grows RPC calls (COW-1162), drain/cancellation concerns land
/// there.
pub fn spawn(
    store: Arc<InMemoryProposalStore>,
    validator: impl ProposalValidator + 'static,
    period: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        loop {
            interval.tick().await;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
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
/// 2. **Validation** — every remaining `Submitted` proposal is judged by the
///    validator and transitioned to `Active`/`Rejected`/`SimFailed`.
///
/// `now` is a unix timestamp from the wall clock; `valid_until` is signed
/// against block timestamps. The drift is seconds at most and only affects
/// when we stop showing/simulating a proposal — the chain enforces the real
/// deadline.
///
/// Works on a single snapshot of all live proposals (one lock acquisition,
/// one scan); each write is a compare-and-swap transition, so a proposal
/// cancelled mid-validation keeps its cancellation (the stale verdict is
/// dropped).
pub async fn run_tick(store: &InMemoryProposalStore, validator: &impl ProposalValidator, now: u64) {
    validator.begin_tick();

    let live = store.snapshot_by_statuses(&[ProposalStatus::Submitted, ProposalStatus::Active]);

    let mut to_validate = Vec::new();
    for proposal in live {
        if proposal.valid_until < alloy::primitives::U256::from(now) {
            match store.transition(proposal.id, proposal.status, ProposalStatus::Expired) {
                Ok(()) => tracing::info!(id = %proposal.id, "proposal expired"),
                Err(e) => tracing::debug!(id = %proposal.id, %e, "stale expiry dropped"),
            }
        } else if proposal.status == ProposalStatus::Submitted {
            to_validate.push(proposal);
        }
    }

    for proposal in to_validate {
        let Some(verdict) = validator.validate(&proposal).await else {
            tracing::debug!(id = %proposal.id, "validator deferred judgment, will retry next tick");
            continue;
        };
        match store.resolve_submitted(proposal.id, verdict) {
            Ok(status) => tracing::info!(id = %proposal.id, %status, "proposal validated"),
            Err(e) => tracing::debug!(id = %proposal.id, %e, "stale verdict dropped"),
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::domain::{
            proposal::{OrderUid, Proposal, ProposalStatus, test_proposal},
            validator::AcceptAll,
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

    /// Store plus its audit receiver — kept alive so emits don't log errors;
    /// these tests assert on statuses, not evidence.
    fn test_store() -> (
        InMemoryProposalStore,
        tokio::sync::mpsc::UnboundedReceiver<crate::domain::audit::AuditEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (InMemoryProposalStore::new(tx), rx)
    }

    #[tokio::test(start_paused = true)]
    async fn spawned_loop_validates_on_its_interval() {
        let (store, _audit) = test_store();
        let store = std::sync::Arc::new(store);
        let id = store.insert(submitted_proposal());

        let _loop = spawn(store.clone(), AcceptAll, std::time::Duration::from_secs(12));
        tokio::time::sleep(std::time::Duration::from_secs(13)).await;

        assert_eq!(
            store.get(id).expect("exists").status,
            ProposalStatus::Active
        );
    }

    #[tokio::test]
    async fn tick_flips_submitted_to_active_with_accept_all() {
        let (store, _audit) = test_store();
        let id = store.insert(submitted_proposal());

        run_tick(&store, &AcceptAll, 0).await;

        assert_eq!(
            store.get(id).expect("exists").status,
            ProposalStatus::Active
        );
    }

    struct FailAll;

    impl ProposalValidator for FailAll {
        async fn validate(
            &self,
            _proposal: &Proposal,
        ) -> Option<crate::domain::validator::Verdict> {
            Some(crate::domain::validator::Verdict::SimFailed)
        }
    }

    #[tokio::test]
    async fn tick_marks_sim_failed_proposals() {
        let (store, _audit) = test_store();
        let id = store.insert(submitted_proposal());

        run_tick(&store, &FailAll, 0).await;

        let proposal = store.get(id).expect("exists");
        assert_eq!(proposal.status, ProposalStatus::SimFailed);
        assert_eq!(proposal.rejection_reason, None);
    }

    #[tokio::test]
    async fn cancellation_during_validation_wins_over_the_verdict() {
        let (store, _audit) = test_store();
        let proposal = submitted_proposal();
        let sub_solver = proposal.sub_solver;
        let id = store.insert(proposal);

        // The owner cancels after the tick snapshotted the proposal but
        // before the verdict lands: applying the verdict must fail and the
        // cancellation must stick.
        store.cancel(id, sub_solver).expect("cancel succeeds");
        let stale = store.resolve_submitted(id, crate::domain::validator::Verdict::Accept);

        assert!(stale.is_err(), "stale verdict must be dropped");
        assert_eq!(
            store.get(id).expect("exists").status,
            ProposalStatus::Cancelled,
            "a stale Accept verdict must not resurrect a cancelled proposal"
        );
    }

    #[tokio::test]
    async fn tick_expires_active_proposals_past_valid_until() {
        let (store, _audit) = test_store();
        let mut proposal = submitted_proposal();
        proposal.status = ProposalStatus::Active;
        proposal.valid_until = U256::from(1_000_u64);
        let id = store.insert(proposal);

        run_tick(&store, &AcceptAll, 1_001).await;

        assert_eq!(
            store.get(id).expect("exists").status,
            ProposalStatus::Expired
        );
    }

    #[tokio::test]
    async fn tick_expires_submitted_proposals_instead_of_validating_them() {
        let (store, _audit) = test_store();
        let mut proposal = submitted_proposal();
        proposal.valid_until = U256::from(1_000_u64);
        let id = store.insert(proposal);

        run_tick(&store, &AcceptAll, 1_001).await;

        assert_eq!(
            store.get(id).expect("exists").status,
            ProposalStatus::Expired
        );
    }
}
