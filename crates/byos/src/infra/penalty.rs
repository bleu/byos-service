//! Track A penalty loop (ADR-0003, ADR-0013, COW-1205): turns `SettleFailed`
//! proposals into landed escrow debits. Each tick scans the pending work and
//! drives the [`crate::domain::penalty::DebitEscrow`] chain edge; a failed
//! debit is retried next tick, up to [`MAX_DEBIT_ATTEMPTS`] times, and the
//! proposal stays queryable in `SettleFailed` until its debit lands.

use {
    crate::{
        domain::{
            penalty::{DebitEscrow, non_settlement_debit, revert_debit},
            proposal::{ProposalId, ProposalStatus},
        },
        infra::storage::ProposalStore,
    },
    alloy::primitives::U256,
    std::{collections::HashMap, sync::Arc, time::Duration},
};

/// A debit that keeps failing is parked after this many attempts. Covers
/// both chain calls a debit needs: a permanently-reverting debit (operator
/// lacks the role, escrow paused or drained) and a settlement we can never
/// price (bad hash, history pruned) must neither retry forever. The
/// giving-up error log is the ops page;
/// counts are in-memory, so a restart re-arms the retries once the cause is
/// fixed.
const MAX_DEBIT_ATTEMPTS: u32 = 10;

/// Per-item debit attempt counts, held by the loop across ticks. An entry is
/// dropped when its debit lands; an item that hits [`MAX_DEBIT_ATTEMPTS`] is
/// skipped (its proposal or queue row stays put) until a restart.
#[derive(Default)]
pub struct DebitAttempts {
    /// Keyed by proposal id — the `SettleFailed` proposal *is* the queue.
    revert: HashMap<ProposalId, u32>,
    /// Keyed by `penalties` row id.
    non_settlement: HashMap<i64, u32>,
}

/// Count a failed debit attempt; logs a per-tick warn while retries remain
/// and one error when the item is parked for good.
fn note_debit_failure(
    attempts: &mut u32,
    id: impl std::fmt::Display,
    e: &crate::domain::penalty::DebitError,
    what: &str,
) {
    *attempts += 1;
    if *attempts >= MAX_DEBIT_ATTEMPTS {
        tracing::error!(
            %id, %e, attempts = *attempts,
            "{what} keeps failing; giving up until restart"
        );
    } else {
        tracing::warn!(%id, %e, "{what} failed; retrying next tick");
    }
}

/// Spawn the penalty loop: one [`run_tick`] every `period`. Like the other
/// background loops it holds the store (and with it an audit sender), so
/// shutdown must abort it before draining the audit writer.
pub fn spawn(
    store: Arc<ProposalStore>,
    operator: impl DebitEscrow + 'static,
    period: Duration,
    c_l: U256,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // First tick a full period out, mirroring the validation loop — a
        // plain `interval` fires immediately and would race startup.
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        // Here the budget, not the load, is what needs protecting: catch-up
        // ticks after an overrun spend `MAX_DEBIT_ATTEMPTS` with no spacing
        // between them, parking a debit that the same attempts spread over
        // minutes would have landed.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut attempts = DebitAttempts::default();
        loop {
            interval.tick().await;
            run_tick(&store, &operator, c_l, &mut attempts).await;
        }
    })
}

/// One pass of the penalty loop, in two sweeps:
///
/// 1. **Revert debits** — every `SettleFailed` proposal is a pending `gas +
///    c_l` debit (the state is the queue — no separate bookkeeping to lose); a
///    landed debit flips it to `Penalized`.
/// 2. **Non-settlement debits** — every pending `penalties` row (queued by
///    `/notify` on a driver-confirmed abandonment) is a `0.1 × c_l` debit.
///
/// Debits run sequentially on purpose: they all spend from the operator
/// account, and concurrent submissions would race its nonce.
pub async fn run_tick(
    store: &ProposalStore,
    operator: &impl DebitEscrow,
    c_l: U256,
    attempts: &mut DebitAttempts,
) {
    revert_debits(store, operator, c_l, &mut attempts.revert).await;
    non_settlement_debits(store, operator, c_l, &mut attempts.non_settlement).await;
}

async fn revert_debits(
    store: &ProposalStore,
    operator: &impl DebitEscrow,
    c_l: U256,
    attempts: &mut HashMap<ProposalId, u32>,
) {
    let pending = match store
        .snapshot_by_statuses(&[ProposalStatus::SettleFailed])
        .await
    {
        Ok(pending) => pending,
        Err(e) => {
            tracing::error!(%e, "penalty tick skipped: snapshot failed");
            return;
        }
    };

    for proposal in pending {
        // Parked: this debit already failed its last allowed attempt.
        if attempts
            .get(&proposal.id)
            .is_some_and(|n| *n >= MAX_DEBIT_ATTEMPTS)
        {
            continue;
        }
        // apply_settlement_outcome always writes the tx alongside
        // SettleFailed, so a missing hash is a corrupt row — alert, don't
        // guess an amount.
        let Some(settlement_tx) = proposal.settlement_tx_hash else {
            tracing::error!(id = %proposal.id, "settleFailed without a settlement tx; cannot debit");
            continue;
        };
        // Counted against the same cap as the debit itself: both are ways
        // this proposal's debit failed to happen, and a receipt we can never
        // fetch (bad hash, history pruned) must not burn an RPC call every
        // tick forever.
        let cost = match operator.settlement_cost(settlement_tx).await {
            Ok(cost) => cost,
            Err(e) => {
                note_debit_failure(
                    attempts.entry(proposal.id).or_insert(0),
                    proposal.id,
                    &e,
                    "settlement cost lookup",
                );
                continue;
            }
        };
        let amount = revert_debit(cost, c_l);
        let penalty_tx = match operator
            .debit(proposal.sub_solver, amount, settlement_tx)
            .await
        {
            Ok(tx) => tx,
            Err(e) => {
                note_debit_failure(
                    attempts.entry(proposal.id).or_insert(0),
                    proposal.id,
                    &e,
                    "escrow debit",
                );
                continue;
            }
        };
        attempts.remove(&proposal.id);
        match store.record_penalty(&proposal, amount, penalty_tx).await {
            Ok(()) => tracing::info!(
                id = %proposal.id, sub_solver = %proposal.sub_solver, %amount,
                penalty_tx = %penalty_tx, "escrow debited; proposal penalized"
            ),
            // The debit tx is on-chain but the proposal still reads
            // SettleFailed — the next tick re-debits, a double charge.
            Err(e) => tracing::error!(
                id = %proposal.id, %e,
                "debit landed but proposal not marked penalized; may re-charge next tick"
            ),
        }
    }
}

async fn non_settlement_debits(
    store: &ProposalStore,
    operator: &impl DebitEscrow,
    c_l: U256,
    attempts: &mut HashMap<i64, u32>,
) {
    let pending = match store.pending_penalties().await {
        Ok(pending) => pending,
        Err(e) => {
            tracing::error!(%e, "non-settlement sweep skipped: queue read failed");
            return;
        }
    };

    for penalty in pending {
        // Parked: this debit already failed its last allowed attempt.
        if attempts
            .get(&penalty.id)
            .is_some_and(|n| *n >= MAX_DEBIT_ATTEMPTS)
        {
            continue;
        }
        let amount = non_settlement_debit(c_l);
        // No settlement tx exists to cite, so the on-chain reason is the
        // order the sub-solver won and abandoned.
        let reason = alloy::primitives::keccak256(penalty.order_uid.0);
        let penalty_tx = match operator.debit(penalty.sub_solver, amount, reason).await {
            Ok(tx) => tx,
            Err(e) => {
                note_debit_failure(
                    attempts.entry(penalty.id).or_insert(0),
                    penalty.proposal_id,
                    &e,
                    "non-settlement debit",
                );
                continue;
            }
        };
        attempts.remove(&penalty.id);
        match store
            .record_non_settlement_debit(&penalty, amount, penalty_tx)
            .await
        {
            Ok(()) => tracing::info!(
                id = %penalty.proposal_id, sub_solver = %penalty.sub_solver, %amount,
                penalty_tx = %penalty_tx, "non-settlement charge debited"
            ),
            Err(e) => tracing::error!(
                id = %penalty.proposal_id, %e,
                "non-settlement debit landed but was not recorded; may re-charge next tick"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            domain::{
                penalty::{DebitError, DebitEscrow},
                proposal::{OrderUid, ProposalStatus, test_proposal},
            },
            tests::setup::TestDb,
        },
        alloy::primitives::{Address, B256, U256, b256},
    };

    /// `c_l` for mainnet: 0.010 ETH.
    const C_L: u64 = 10_000_000_000_000_000;

    const SETTLEMENT_TX: B256 =
        b256!("2222222222222222222222222222222222222222222222222222222222222222");
    const PENALTY_TX: B256 =
        b256!("7777777777777777777777777777777777777777777777777777777777777777");

    /// Store on a fresh database; the audit receiver is leaked to keep the
    /// channel open (these tests assert on statuses, not evidence).
    async fn test_store() -> ProposalStore {
        let db = TestDb::create().await;
        let pool = crate::infra::audit::connect_and_migrate(&db.url)
            .await
            .expect("migrations run");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::mem::forget(rx);
        ProposalStore::new(pool, tx)
    }

    /// Chain edge that answers every receipt lookup with a fixed cost and
    /// lands every debit as [`PENALTY_TX`], recording what it was asked to
    /// charge.
    struct StubOperator {
        settlement_cost: U256,
        debits: parking_lot::Mutex<Vec<(Address, U256, B256)>>,
    }

    impl StubOperator {
        fn new(settlement_cost: U256) -> Self {
            Self {
                settlement_cost,
                debits: parking_lot::Mutex::new(vec![]),
            }
        }
    }

    impl DebitEscrow for StubOperator {
        async fn settlement_cost(&self, _tx: B256) -> Result<U256, DebitError> {
            Ok(self.settlement_cost)
        }

        async fn debit(
            &self,
            sub_solver: Address,
            amount: U256,
            reason: B256,
        ) -> Result<B256, DebitError> {
            self.debits.lock().push((sub_solver, amount, reason));
            Ok(PENALTY_TX)
        }
    }

    /// Chain edge whose debit fails a set number of times before landing —
    /// the "operator account empty / RPC down" shape the loop must retry.
    struct FlakyOperator {
        failures_left: std::sync::atomic::AtomicU32,
    }

    impl DebitEscrow for FlakyOperator {
        async fn settlement_cost(&self, _tx: B256) -> Result<U256, DebitError> {
            Ok(U256::from(1_000u64))
        }

        async fn debit(
            &self,
            _sub_solver: Address,
            _amount: U256,
            _reason: B256,
        ) -> Result<B256, DebitError> {
            use std::sync::atomic::Ordering;
            if self
                .failures_left
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err(DebitError::Transient("debit tx not mined".into()));
            }
            Ok(PENALTY_TX)
        }
    }

    /// Chain edge whose debit never lands — the "operator lacks the role,
    /// escrow paused" shape the loop must eventually give up on. Counts how
    /// many times it was asked.
    #[derive(Default)]
    struct DeadOperator {
        calls: std::sync::atomic::AtomicU32,
    }

    impl DeadOperator {
        fn calls(&self) -> u32 {
            self.calls.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl DebitEscrow for DeadOperator {
        async fn settlement_cost(&self, _tx: B256) -> Result<U256, DebitError> {
            Ok(U256::from(1_000u64))
        }

        async fn debit(
            &self,
            _sub_solver: Address,
            _amount: U256,
            _reason: B256,
        ) -> Result<B256, DebitError> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(DebitError::Transient("operator lacks OPERATOR_ROLE".into()))
        }
    }

    /// A `SettleFailed` proposal with no settlement tx must not be debited.
    ///
    /// `record_outcome` always writes the hash alongside `SettleFailed`, so a
    /// missing one is a corrupt row — and the amount depends on the
    /// settlement's on-chain cost, so there is nothing to charge. Every other
    /// test sets the field, which left the guard uncovered: a change that
    /// substituted a default hash would price the debit off an unrelated (or
    /// zero) transaction and charge a real address for it.
    #[ignore]
    #[tokio::test]
    async fn a_settle_failed_proposal_without_a_settlement_tx_is_not_debited() {
        let store = test_store().await;
        let proposal = test_proposal(
            OrderUid([0xaa; 56]),
            Address::repeat_byte(0x01),
            ProposalStatus::SettleFailed,
        );
        // Deliberately no settlement_tx_hash.
        let id = store.insert(proposal).await.expect("insert");

        let operator = StubOperator::new(U256::from(6_000_000_000_000_000u64));
        let mut attempts = DebitAttempts::default();
        run_tick(&store, &operator, U256::from(C_L), &mut attempts).await;

        assert!(
            operator.debits.lock().is_empty(),
            "no amount can be derived without the settlement, so nothing may be charged"
        );
        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::SettleFailed,
            "the proposal stays put for a human rather than being marked penalized"
        );
    }

    #[ignore]
    #[tokio::test]
    async fn spawned_loop_debits_on_its_interval() {
        let store = Arc::new(test_store().await);
        let mut proposal = test_proposal(
            OrderUid([0xaa; 56]),
            Address::repeat_byte(0x01),
            ProposalStatus::SettleFailed,
        );
        proposal.settlement_tx_hash = Some(SETTLEMENT_TX);
        let id = store.insert(proposal).await.expect("insert");

        let _loop = spawn(
            store.clone(),
            StubOperator::new(U256::ZERO),
            Duration::from_millis(50),
            U256::from(C_L),
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let status = store.get(id).await.expect("get").expect("exists").status;
            if status == ProposalStatus::Penalized {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "loop never debited the proposal, still {status}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Acceptance (COW-1205): a queued non-settlement charge is debited at
    /// 0.1 × c_l with the order UID hash as the on-chain reason (no
    /// settlement tx exists), exactly once — and the proposal itself stays
    /// `Active`, still competing (losses are events, not states, ADR-0013).
    #[ignore]
    #[tokio::test]
    async fn tick_debits_queued_non_settlement_penalties_once() {
        let store = test_store().await;
        let order_uid = OrderUid([0xaa; 56]);
        let proposal = test_proposal(
            order_uid.clone(),
            Address::repeat_byte(0x01),
            ProposalStatus::Active,
        );
        let id = store.insert(proposal).await.expect("insert");
        let stored = store.get(id).await.expect("get").expect("exists");
        store
            .queue_non_settlement_penalty(&stored)
            .await
            .expect("queue");

        let operator = StubOperator::new(U256::ZERO);
        let mut attempts = DebitAttempts::default();
        run_tick(&store, &operator, U256::from(C_L), &mut attempts).await;

        assert_eq!(
            *operator.debits.lock(),
            vec![(
                Address::repeat_byte(0x01),
                U256::from(C_L / 10),
                alloy::primitives::keccak256(order_uid.0),
            )],
            "0.1 × c_l, citing the order the sub-solver won and abandoned"
        );
        assert!(
            store.pending_penalties().await.expect("pending").is_empty(),
            "a landed charge leaves the pending queue"
        );
        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Active,
            "the proposal keeps competing; only its escrow is charged"
        );

        run_tick(&store, &operator, U256::from(C_L), &mut attempts).await;
        assert_eq!(
            operator.debits.lock().len(),
            1,
            "a debited charge is never re-charged"
        );
    }

    /// Acceptance (COW-1205): a failed debit leaves the proposal queryable
    /// in `SettleFailed`; the next tick retries and lands it.
    #[ignore]
    #[tokio::test]
    async fn failed_debit_stays_settle_failed_until_a_later_tick_lands_it() {
        let store = test_store().await;
        let mut proposal = test_proposal(
            OrderUid([0xaa; 56]),
            Address::repeat_byte(0x01),
            ProposalStatus::SettleFailed,
        );
        proposal.settlement_tx_hash = Some(SETTLEMENT_TX);
        let id = store.insert(proposal).await.expect("insert");

        let operator = FlakyOperator {
            failures_left: std::sync::atomic::AtomicU32::new(1),
        };
        let mut attempts = DebitAttempts::default();

        run_tick(&store, &operator, U256::from(C_L), &mut attempts).await;
        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::SettleFailed,
            "an unlanded debit must keep the proposal queryable in settleFailed (ADR-0013)"
        );

        run_tick(&store, &operator, U256::from(C_L), &mut attempts).await;
        let proposal = store.get(id).await.expect("get").expect("exists");
        assert_eq!(proposal.status, ProposalStatus::Penalized);
        assert_eq!(proposal.penalty_tx_hash, Some(PENALTY_TX));
    }

    /// A debit that can never land is parked after [`MAX_DEBIT_ATTEMPTS`]
    /// tries instead of retrying every tick forever. The proposal stays in
    /// `SettleFailed`, so a restart re-arms it once the cause is fixed.
    #[ignore]
    #[tokio::test]
    async fn a_permanently_failing_revert_debit_is_given_up_on() {
        let store = test_store().await;
        let mut proposal = test_proposal(
            OrderUid([0xaa; 56]),
            Address::repeat_byte(0x01),
            ProposalStatus::SettleFailed,
        );
        proposal.settlement_tx_hash = Some(SETTLEMENT_TX);
        let id = store.insert(proposal).await.expect("insert");

        let operator = DeadOperator::default();
        let mut attempts = DebitAttempts::default();
        for _ in 0..MAX_DEBIT_ATTEMPTS + 5 {
            run_tick(&store, &operator, U256::from(C_L), &mut attempts).await;
        }

        assert_eq!(
            operator.calls(),
            MAX_DEBIT_ATTEMPTS,
            "the loop must stop submitting after the attempt cap"
        );
        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::SettleFailed,
            "a given-up debit leaves the proposal queryable, not silently penalized"
        );
    }

    /// Chain edge whose receipt lookup never succeeds — a settlement tx we
    /// can never price (bad hash, history pruned on the node). Counts the
    /// lookups so the attempt cap is observable.
    #[derive(Default)]
    struct UnpriceableOperator {
        lookups: std::sync::atomic::AtomicU32,
    }

    impl DebitEscrow for UnpriceableOperator {
        async fn settlement_cost(&self, _tx: B256) -> Result<U256, DebitError> {
            self.lookups
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(DebitError::Transient("no receipt for that tx".into()))
        }

        async fn debit(
            &self,
            _sub_solver: Address,
            _amount: U256,
            _reason: B256,
        ) -> Result<B256, DebitError> {
            panic!("must not debit a settlement it could not price")
        }
    }

    /// A cost lookup that can never succeed is capped like a failing debit —
    /// otherwise an unpriceable settlement burns one RPC call every tick for
    /// the life of the process.
    #[ignore]
    #[tokio::test]
    async fn a_permanently_unpriceable_settlement_stops_being_looked_up() {
        let store = test_store().await;
        let mut proposal = test_proposal(
            OrderUid([0xaa; 56]),
            Address::repeat_byte(0x01),
            ProposalStatus::SettleFailed,
        );
        proposal.settlement_tx_hash = Some(SETTLEMENT_TX);
        let id = store.insert(proposal).await.expect("insert");

        let operator = UnpriceableOperator::default();
        let mut attempts = DebitAttempts::default();
        for _ in 0..MAX_DEBIT_ATTEMPTS + 5 {
            run_tick(&store, &operator, U256::from(C_L), &mut attempts).await;
        }

        assert_eq!(
            operator.lookups.load(std::sync::atomic::Ordering::Relaxed),
            MAX_DEBIT_ATTEMPTS,
            "the cost lookup must stop after the attempt cap, not retry forever"
        );
        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::SettleFailed,
            "a parked debit leaves the proposal queryable, not silently penalized"
        );
    }

    /// The same cap guards the non-settlement queue: a charge that cannot be
    /// paid stops being retried, and its row stays pending for a restart.
    #[ignore]
    #[tokio::test]
    async fn a_permanently_failing_non_settlement_debit_is_given_up_on() {
        let store = test_store().await;
        let proposal = test_proposal(
            OrderUid([0xaa; 56]),
            Address::repeat_byte(0x01),
            ProposalStatus::Active,
        );
        let id = store.insert(proposal).await.expect("insert");
        let stored = store.get(id).await.expect("get").expect("exists");
        store
            .queue_non_settlement_penalty(&stored)
            .await
            .expect("queue");

        let operator = DeadOperator::default();
        let mut attempts = DebitAttempts::default();
        for _ in 0..MAX_DEBIT_ATTEMPTS + 5 {
            run_tick(&store, &operator, U256::from(C_L), &mut attempts).await;
        }

        assert_eq!(
            operator.calls(),
            MAX_DEBIT_ATTEMPTS,
            "the loop must stop submitting after the attempt cap"
        );
        assert_eq!(
            store.pending_penalties().await.expect("pending").len(),
            1,
            "the unpaid charge stays queued for a later restart"
        );
    }

    /// Acceptance (COW-1205): a `SettleFailed` proposal ends `Penalized`
    /// with the debit tx recorded once the escrow debit lands.
    #[ignore]
    #[tokio::test]
    async fn tick_debits_a_settle_failed_proposal_and_marks_it_penalized() {
        let store = test_store().await;
        let mut proposal = test_proposal(
            OrderUid([0xaa; 56]),
            Address::repeat_byte(0x01),
            ProposalStatus::SettleFailed,
        );
        proposal.settlement_tx_hash = Some(SETTLEMENT_TX);
        let id = store.insert(proposal).await.expect("insert");

        let operator = StubOperator::new(U256::from(6_000_000_000_000_000u64)); // 200k gas × 30 gwei
        run_tick(
            &store,
            &operator,
            U256::from(C_L),
            &mut DebitAttempts::default(),
        )
        .await;

        let proposal = store.get(id).await.expect("get").expect("exists");
        assert_eq!(proposal.status, ProposalStatus::Penalized);
        assert_eq!(
            proposal.penalty_tx_hash,
            Some(PENALTY_TX),
            "the proposal row must cite the landed debit tx (ADR-0013)"
        );
        assert_eq!(
            *operator.debits.lock(),
            vec![(
                Address::repeat_byte(0x01),
                // Acceptance (COW-1205): receipt gas cost + c_l.
                U256::from(16_000_000_000_000_000u64),
                SETTLEMENT_TX,
            )],
            "one debit of gas + c_l, citing the reverted settlement (ADR-0003)"
        );
    }
}
