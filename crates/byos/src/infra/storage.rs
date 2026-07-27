//! Postgres-backed proposal store (ADR-0013): the `proposals` table is the
//! single source of truth for current state. Ingestion, reads, cancellation,
//! the background validator, and `/solve` all read and write it directly —
//! no cache layer. The table holds what *is*; `audit_events` holds what
//! *happened*.
//!
//! Every mutation emits an [`audit::AuditEvent`] — auditing happens by
//! construction, so future mutation sites cannot forget to leave evidence.

use {
    crate::domain::{
        audit,
        proposal::{OrderUid, Proposal, ProposalId, ProposalStatus},
        validator::RejectionReason,
    },
    alloy::primitives::{Address, Bytes, U256},
    byos_common::contracts::Interaction,
    sqlx::postgres::PgPool,
    std::{sync::Arc, time::SystemTime},
};

/// Store-level error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    #[error("proposal {0} not found")]
    NotFound(ProposalId),
    #[error("proposal {0} not owned by {1}")]
    NotOwner(ProposalId, Address),
    #[error("proposal {id} is {actual}, expected {expected}")]
    StaleTransition {
        id: ProposalId,
        expected: String,
        actual: ProposalStatus,
    },
    #[error("database error")]
    Database(#[from] sqlx::Error),
}

pub struct ProposalStore {
    pool: PgPool,
    audit: audit::Sender,
}

impl ProposalStore {
    pub fn new(pool: PgPool, audit: audit::Sender) -> Self {
        Self { pool, audit }
    }

    /// The audit channel is unbounded, so a send only fails if the writer
    /// task is gone — a bug, not a runtime condition; log loudly.
    fn emit(&self, event: audit::AuditEvent) {
        if let Err(err) = self.audit.send(event) {
            tracing::error!(
                proposal_id = %err.0.proposal_id(),
                "audit writer gone; evidence event dropped"
            );
        }
    }

    /// Insert a proposal. The `id` field on the input is ignored — the
    /// database sequence assigns a fresh one (unique across restarts) and
    /// the store returns it.
    pub async fn insert(&self, mut proposal: Proposal) -> Result<ProposalId, StoreError> {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO proposals (sub_solver, order_uid, order_uid_hash, sell_amount, \
             buy_amount, sell_token, buy_token, interactions, interactions_hash, valid_until, \
             nonce, signature, status, rejection_reason, gas_used, trampoline) VALUES ($1, $2, \
             $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) RETURNING id",
        )
        .bind(format!("{:#x}", proposal.sub_solver))
        .bind(proposal.order_uid.to_string())
        .bind(format!("{:#x}", proposal.order_uid_hash))
        .bind(proposal.sell_amount.to_string())
        .bind(proposal.buy_amount.to_string())
        .bind(format!("{:#x}", proposal.sell_token))
        .bind(format!("{:#x}", proposal.buy_token))
        .bind(interactions_to_json(&proposal.interactions))
        .bind(format!("{:#x}", proposal.interactions_hash))
        .bind(proposal.valid_until.to_string())
        .bind(proposal.nonce.to_string())
        .bind(proposal.signature.to_string())
        .bind(proposal.status.to_string())
        .bind(proposal.rejection_reason.map(|r| r.to_string()))
        .bind(
            proposal
                .gas_used
                .map(|g| i64::try_from(g).expect("gas exceeds i64")),
        )
        .bind(proposal.trampoline.map(|t| format!("{t:#x}")))
        .fetch_one(&self.pool)
        .await?;

        proposal.id = ProposalId(u64::try_from(id).expect("sequence ids are positive"));
        let arc = Arc::new(proposal);
        let id = arc.id;
        self.emit(audit::AuditEvent {
            occurred_at: SystemTime::now(),
            kind: audit::AuditKind::Received { proposal: arc },
        });
        Ok(id)
    }

    /// Transition a proposal from `from` to `to`, only if it is still in
    /// `from` — one compare-and-swap `UPDATE`. Zero rows affected means
    /// someone else (e.g. a cancellation) won the race: the caller's write
    /// is stale and must be dropped. A successful transition emits a
    /// status-changed audit event.
    pub async fn transition(
        &self,
        id: ProposalId,
        from: ProposalStatus,
        to: ProposalStatus,
    ) -> Result<(), StoreError> {
        let row: Option<(String, String)> = sqlx::query_as(
            "UPDATE proposals SET status = $3, status_changed_at = now() WHERE id = $1 AND status \
             = $2 RETURNING sub_solver, order_uid",
        )
        .bind(as_db_id(id))
        .bind(from.to_string())
        .bind(to.to_string())
        .fetch_optional(&self.pool)
        .await?;

        let Some((sub_solver, order_uid)) = row else {
            return Err(self.stale_or_missing(id, from.to_string()).await);
        };

        self.emit(audit::AuditEvent {
            occurred_at: SystemTime::now(),
            kind: audit::AuditKind::StatusChanged {
                proposal_id: id,
                sub_solver: sub_solver.parse().map_err(|e| corrupt("sub_solver", e))?,
                order_uid: order_uid.parse().map_err(|e| corrupt("order_uid", e))?,
                from,
                to,
                rejection_reason: None,
            },
        });
        Ok(())
    }

    /// Apply a validator verdict to a `Submitted` or `Active` proposal.
    ///
    /// - `Accept`: transitions `Submitted` → `Active`, or keeps `Active` →
    ///   `Active` (re-validation). Writes the simulation outcome (gas,
    ///   trampoline, tokens) onto the proposal when the verdict carries one.
    /// - `Reject`: transitions to `Rejected` with a rejection reason.
    /// - `SimFailed`: transitions to `SimFailed`.
    ///
    /// Fails with `StaleTransition` if the proposal is not in `Submitted` or
    /// `Active` (e.g. a cancellation raced the validator). `Active` →
    /// `Active` re-validation updates the simulation data but emits no audit
    /// event — emitting on every tick would be noisy.
    pub async fn resolve_verdict(
        &self,
        id: ProposalId,
        verdict: crate::domain::validator::Verdict,
    ) -> Result<ProposalStatus, StoreError> {
        use crate::domain::validator::Verdict;

        // A short row-locked transaction keeps the compare-and-swap
        // semantics: a racing cancellation either commits before the lock
        // (this verdict turns stale) or waits behind it.
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT status, sub_solver, order_uid FROM proposals WHERE id = $1 FOR UPDATE",
        )
        .bind(as_db_id(id))
        .fetch_optional(&mut *tx)
        .await?;

        let Some((status, sub_solver, order_uid)) = row else {
            return Err(StoreError::NotFound(id));
        };
        let from: ProposalStatus = status.parse().map_err(|e| corrupt("status", e))?;
        if !matches!(from, ProposalStatus::Submitted | ProposalStatus::Active) {
            return Err(StoreError::StaleTransition {
                id,
                expected: "submitted or active".into(),
                actual: from,
            });
        }

        let (to, rejection_reason) = match verdict {
            Verdict::Accept(_) => (ProposalStatus::Active, None),
            Verdict::Reject(reason) => (ProposalStatus::Rejected, Some(reason)),
            Verdict::SimFailed => (ProposalStatus::SimFailed, None),
        };
        let sim = match verdict {
            Verdict::Accept(sim) => sim,
            Verdict::Reject(_) | Verdict::SimFailed => None,
        };

        sqlx::query(
            "UPDATE proposals SET status = $2, rejection_reason = $3, gas_used = COALESCE($4, \
             gas_used), trampoline = COALESCE($5, trampoline), sell_token = COALESCE($6, \
             sell_token), buy_token = COALESCE($7, buy_token), status_changed_at = CASE WHEN \
             status = $2 THEN status_changed_at ELSE now() END WHERE id = $1",
        )
        .bind(as_db_id(id))
        .bind(to.to_string())
        .bind(rejection_reason.map(|r| r.to_string()))
        .bind(sim.map(|s| i64::try_from(s.gas_used).expect("gas exceeds i64")))
        .bind(sim.map(|s| format!("{:#x}", s.trampoline)))
        .bind(sim.map(|s| format!("{:#x}", s.sell_token)))
        .bind(sim.map(|s| format!("{:#x}", s.buy_token)))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        if from != to {
            self.emit(audit::AuditEvent {
                occurred_at: SystemTime::now(),
                kind: audit::AuditKind::StatusChanged {
                    proposal_id: id,
                    sub_solver: sub_solver.parse().map_err(|e| corrupt("sub_solver", e))?,
                    order_uid: order_uid.parse().map_err(|e| corrupt("order_uid", e))?,
                    from,
                    to,
                    rejection_reason,
                },
            });
        }
        Ok(to)
    }

    /// Cancel a proposal. Only live proposals (`Submitted`/`Active`) can be
    /// cancelled; returns `Err` if not found, not owned by the given
    /// sub-solver, or already in a terminal state.
    pub async fn cancel(&self, id: ProposalId, sub_solver: Address) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT status, sub_solver, order_uid FROM proposals WHERE id = $1 FOR UPDATE",
        )
        .bind(as_db_id(id))
        .fetch_optional(&mut *tx)
        .await?;

        let Some((status, owner, order_uid)) = row else {
            return Err(StoreError::NotFound(id));
        };
        if owner != format!("{sub_solver:#x}") {
            return Err(StoreError::NotOwner(id, sub_solver));
        }
        let from: ProposalStatus = status.parse().map_err(|e| corrupt("status", e))?;
        if !matches!(from, ProposalStatus::Submitted | ProposalStatus::Active) {
            return Err(StoreError::StaleTransition {
                id,
                expected: "submitted or active".into(),
                actual: from,
            });
        }

        sqlx::query("UPDATE proposals SET status = $2, status_changed_at = now() WHERE id = $1")
            .bind(as_db_id(id))
            .bind(ProposalStatus::Cancelled.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        self.emit(audit::AuditEvent {
            occurred_at: SystemTime::now(),
            kind: audit::AuditKind::Cancelled {
                proposal_id: id,
                sub_solver,
                order_uid: order_uid.parse().map_err(|e| corrupt("order_uid", e))?,
            },
        });
        Ok(())
    }

    /// Disambiguate a zero-row compare-and-swap: the proposal is either gone
    /// (`NotFound`) or sits in a different status (`StaleTransition`).
    async fn stale_or_missing(&self, id: ProposalId, expected: String) -> StoreError {
        let status: Option<String> =
            match sqlx::query_scalar("SELECT status FROM proposals WHERE id = $1")
                .bind(as_db_id(id))
                .fetch_optional(&self.pool)
                .await
            {
                Ok(status) => status,
                Err(e) => return StoreError::Database(e),
            };
        match status {
            None => StoreError::NotFound(id),
            Some(actual) => match actual.parse() {
                Ok(actual) => StoreError::StaleTransition {
                    id,
                    expected,
                    actual,
                },
                Err(e) => corrupt("status", e),
            },
        }
    }

    /// List active proposals for a given order UID — the `/solve` view.
    pub async fn list_by_order_uid(
        &self,
        order_uid: &OrderUid,
    ) -> Result<Vec<Proposal>, StoreError> {
        let rows: Vec<ProposalRow> = sqlx::query_as(&format!(
            "SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE order_uid = $1 AND status = $2 ORDER \
             BY id"
        ))
        .bind(order_uid.to_string())
        .bind(ProposalStatus::Active.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Proposal::try_from).collect()
    }

    /// List live (`Submitted` or `Active`) proposals for a given sub-solver
    /// address. This is the owner's management view, so pending submissions
    /// are included.
    pub async fn list_by_sub_solver(
        &self,
        sub_solver: Address,
    ) -> Result<Vec<Proposal>, StoreError> {
        let rows: Vec<ProposalRow> = sqlx::query_as(&format!(
            "SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE sub_solver = $1 AND status = ANY($2) \
             ORDER BY id"
        ))
        .bind(format!("{sub_solver:#x}"))
        .bind(status_names(&[
            ProposalStatus::Submitted,
            ProposalStatus::Active,
        ]))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Proposal::try_from).collect()
    }

    /// Every proposal currently in one of the given statuses — the
    /// background validator's per-tick working set.
    pub async fn snapshot_by_statuses(
        &self,
        statuses: &[ProposalStatus],
    ) -> Result<Vec<Proposal>, StoreError> {
        let rows: Vec<ProposalRow> = sqlx::query_as(&format!(
            "SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE status = ANY($1) ORDER BY id"
        ))
        .bind(status_names(statuses))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Proposal::try_from).collect()
    }

    /// Look up a single proposal by ID.
    pub async fn get(&self, id: ProposalId) -> Result<Option<Proposal>, StoreError> {
        let row: Option<ProposalRow> = sqlx::query_as(&format!(
            "SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE id = $1"
        ))
        .bind(as_db_id(id))
        .fetch_optional(&self.pool)
        .await?;
        row.map(Proposal::try_from).transpose()
    }
}

/// A `ProposalId` as the BIGINT the `id` column holds.
fn as_db_id(id: ProposalId) -> i64 {
    i64::try_from(id.0).expect("proposal id exceeds i64")
}

/// Status strings for a `= ANY($n)` bind.
fn status_names(statuses: &[ProposalStatus]) -> Vec<String> {
    statuses.iter().map(|s| s.to_string()).collect()
}

// ---------------------------------------------------------------------------
// Row codec
// ---------------------------------------------------------------------------

const PROPOSAL_COLUMNS: &str = "id, sub_solver, order_uid, order_uid_hash, sell_amount, \
                                buy_amount, sell_token, buy_token, interactions, \
                                interactions_hash, valid_until, nonce, signature, status, \
                                rejection_reason, gas_used, trampoline";

/// The raw column values; [`Proposal::try_from`] parses them back into
/// domain types. A parse failure means a corrupt row (we wrote these
/// values), surfaced as a `StoreError::Database`.
#[derive(sqlx::FromRow)]
struct ProposalRow {
    id: i64,
    sub_solver: String,
    order_uid: String,
    order_uid_hash: String,
    sell_amount: String,
    buy_amount: String,
    sell_token: String,
    buy_token: String,
    interactions: serde_json::Value,
    interactions_hash: String,
    valid_until: String,
    nonce: String,
    signature: String,
    status: String,
    rejection_reason: Option<String>,
    gas_used: Option<i64>,
    trampoline: Option<String>,
}

/// Wrap a column parse failure as a database error.
fn corrupt(column: &str, err: impl std::fmt::Display) -> StoreError {
    StoreError::Database(sqlx::Error::Decode(
        format!("corrupt proposals.{column}: {err}").into(),
    ))
}

impl TryFrom<ProposalRow> for Proposal {
    type Error = StoreError;

    fn try_from(row: ProposalRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: ProposalId(u64::try_from(row.id).map_err(|e| corrupt("id", e))?),
            sub_solver: row
                .sub_solver
                .parse()
                .map_err(|e| corrupt("sub_solver", e))?,
            order_uid: row.order_uid.parse().map_err(|e| corrupt("order_uid", e))?,
            order_uid_hash: row
                .order_uid_hash
                .parse()
                .map_err(|e| corrupt("order_uid_hash", e))?,
            sell_amount: parse_u256(&row.sell_amount).map_err(|e| corrupt("sell_amount", e))?,
            buy_amount: parse_u256(&row.buy_amount).map_err(|e| corrupt("buy_amount", e))?,
            sell_token: row
                .sell_token
                .parse()
                .map_err(|e| corrupt("sell_token", e))?,
            buy_token: row.buy_token.parse().map_err(|e| corrupt("buy_token", e))?,
            interactions: interactions_from_json(&row.interactions)
                .map_err(|e| corrupt("interactions", e))?,
            interactions_hash: row
                .interactions_hash
                .parse()
                .map_err(|e| corrupt("interactions_hash", e))?,
            valid_until: parse_u256(&row.valid_until).map_err(|e| corrupt("valid_until", e))?,
            nonce: parse_u256(&row.nonce).map_err(|e| corrupt("nonce", e))?,
            signature: row.signature.parse().map_err(|e| corrupt("signature", e))?,
            status: row.status.parse().map_err(|e| corrupt("status", e))?,
            rejection_reason: row
                .rejection_reason
                .map(|r| r.parse::<RejectionReason>())
                .transpose()
                .map_err(|e| corrupt("rejection_reason", e))?,
            gas_used: row
                .gas_used
                .map(|g| u64::try_from(g).map_err(|e| corrupt("gas_used", e)))
                .transpose()?,
            trampoline: row
                .trampoline
                .map(|t| t.parse::<Address>())
                .transpose()
                .map_err(|e| corrupt("trampoline", e))?,
        })
    }
}

fn parse_u256(s: &str) -> Result<U256, alloy::primitives::ruint::ParseError> {
    U256::from_str_radix(s, 10)
}

/// `[{target, value, callData}]` — the same shape the audit payload uses,
/// so evidence and state read alike.
fn interactions_to_json(interactions: &[Interaction]) -> serde_json::Value {
    serde_json::Value::Array(
        interactions
            .iter()
            .map(|i| {
                serde_json::json!({
                    "target": i.target,
                    "value": i.value.to_string(),
                    "callData": i.callData,
                })
            })
            .collect(),
    )
}

fn interactions_from_json(value: &serde_json::Value) -> Result<Vec<Interaction>, String> {
    let items = value.as_array().ok_or("expected a JSON array")?;
    items
        .iter()
        .map(|item| {
            let field = |name: &str| {
                item.get(name)
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("missing field {name}"))
            };
            Ok(Interaction {
                target: field("target")?
                    .parse::<Address>()
                    .map_err(|e| e.to_string())?,
                value: parse_u256(field("value")?).map_err(|e| e.to_string())?,
                callData: field("callData")?
                    .parse::<Bytes>()
                    .map_err(|e| e.to_string())?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            domain::{
                audit::AuditEvent,
                proposal::{ProposalStatus, test_proposal},
                validator::{SimulationOutcome, Verdict},
            },
            tests::setup::TestDb,
        },
        alloy::primitives::{Address, address},
        tokio::sync::mpsc,
    };

    const SOLVER_A: Address = address!("0000000000000000000000000000000000000001");
    const SOLVER_B: Address = address!("0000000000000000000000000000000000000002");

    /// A fresh store on a fresh database, plus the audit receiver so tests
    /// can assert on emitted evidence.
    async fn test_store() -> (ProposalStore, mpsc::UnboundedReceiver<AuditEvent>) {
        let db = TestDb::create().await;
        let pool = crate::infra::audit::connect_and_migrate(&db.url)
            .await
            .expect("migrations run");
        let (tx, rx) = mpsc::unbounded_channel();
        (ProposalStore::new(pool, tx), rx)
    }

    fn test_order_uid() -> OrderUid {
        OrderUid([0xaa; 56])
    }

    fn make_proposal(order_uid: OrderUid, sub_solver: Address) -> Proposal {
        test_proposal(order_uid, sub_solver, ProposalStatus::Active)
    }

    #[ignore]
    #[tokio::test]
    async fn insert_and_get_round_trips_every_field() {
        let (store, _audit) = test_store().await;
        let mut proposal = make_proposal(test_order_uid(), SOLVER_A);
        proposal.interactions = vec![byos_common::contracts::Interaction {
            target: address!("00000000000000000000000000000000000000bb"),
            value: alloy::primitives::U256::from(5u64),
            callData: alloy::primitives::bytes!("deadbeef"),
        }];
        proposal.sell_amount = alloy::primitives::U256::MAX;
        proposal.signature = alloy::primitives::Bytes::from(vec![0x11; 65]);
        let expected = proposal.clone();

        let id = store.insert(proposal).await.expect("insert succeeds");
        assert!(id.0 > 0);

        let fetched = store.get(id).await.expect("get succeeds").expect("exists");
        assert_eq!(fetched.id, id);
        assert_eq!(fetched.sub_solver, expected.sub_solver);
        assert_eq!(fetched.order_uid, expected.order_uid);
        assert_eq!(fetched.order_uid_hash, expected.order_uid_hash);
        assert_eq!(fetched.sell_amount, expected.sell_amount);
        assert_eq!(fetched.buy_amount, expected.buy_amount);
        assert_eq!(fetched.sell_token, expected.sell_token);
        assert_eq!(fetched.buy_token, expected.buy_token);
        assert_eq!(fetched.interactions, expected.interactions);
        assert_eq!(fetched.interactions_hash, expected.interactions_hash);
        assert_eq!(fetched.valid_until, expected.valid_until);
        assert_eq!(fetched.nonce, expected.nonce);
        assert_eq!(fetched.signature, expected.signature);
        assert_eq!(fetched.status, expected.status);
        assert_eq!(fetched.rejection_reason, None);
        assert_eq!(fetched.gas_used, None);
        assert_eq!(fetched.trampoline, None);
    }

    #[ignore]
    #[tokio::test]
    async fn get_nonexistent_returns_none() {
        let (store, _audit) = test_store().await;
        assert!(
            store
                .get(ProposalId(999))
                .await
                .expect("query ok")
                .is_none()
        );
    }

    #[ignore]
    #[tokio::test]
    async fn insert_emits_received_audit_event() {
        let (store, mut audit) = test_store().await;

        let id = store
            .insert(make_proposal(test_order_uid(), SOLVER_A))
            .await
            .expect("insert succeeds");

        let event = audit.try_recv().expect("insert should emit an audit event");
        assert_eq!(event.proposal_id(), id);
        assert_eq!(event.sub_solver(), SOLVER_A);
        assert_eq!(*event.order_uid(), test_order_uid());
        match event.kind {
            crate::domain::audit::AuditKind::Received { proposal } => {
                assert_eq!(proposal.id, id, "audited body must carry the assigned id");
            }
            other => panic!("expected Received, got {other:?}"),
        }
    }

    #[ignore]
    #[tokio::test]
    async fn transition_updates_status_and_emits_event() {
        let (store, mut audit) = test_store().await;
        let id = store
            .insert(make_proposal(test_order_uid(), SOLVER_A))
            .await
            .expect("insert");
        let _received = audit.try_recv().expect("insert event");

        store
            .transition(id, ProposalStatus::Active, ProposalStatus::Expired)
            .await
            .expect("transition lands");

        let fetched = store.get(id).await.expect("get").expect("exists");
        assert_eq!(fetched.status, ProposalStatus::Expired);

        let event = audit.try_recv().expect("transition should emit an event");
        assert_eq!(event.proposal_id(), id);
        assert_eq!(event.event_type(), "expired");
    }

    #[ignore]
    #[tokio::test]
    async fn stale_transition_emits_nothing() {
        let (store, mut audit) = test_store().await;
        let id = store
            .insert(make_proposal(test_order_uid(), SOLVER_A))
            .await
            .expect("insert");
        let _received = audit.try_recv().expect("insert event");

        // The proposal is Active; a transition expecting Submitted is stale.
        let err = store
            .transition(id, ProposalStatus::Submitted, ProposalStatus::Expired)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::StaleTransition { .. }));
        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Active,
            "a stale transition must not change the row"
        );
        assert!(
            audit.try_recv().is_err(),
            "a dropped transition must not leave evidence"
        );
    }

    #[ignore]
    #[tokio::test]
    async fn reject_verdict_records_the_reason() {
        let (store, mut audit) = test_store().await;
        let id = store
            .insert(test_proposal(
                test_order_uid(),
                SOLVER_A,
                ProposalStatus::Submitted,
            ))
            .await
            .expect("insert");
        let _received = audit.try_recv().expect("insert event");

        let reason = RejectionReason::InsufficientEscrow;
        let status = store
            .resolve_verdict(id, Verdict::Reject(reason))
            .await
            .expect("verdict lands");
        assert_eq!(status, ProposalStatus::Rejected);

        let fetched = store.get(id).await.expect("get").expect("exists");
        assert_eq!(fetched.status, ProposalStatus::Rejected);
        assert_eq!(fetched.rejection_reason, Some(reason));

        let event = audit.try_recv().expect("verdict should emit an event");
        assert_eq!(event.event_type(), "rejected");
        match event.kind {
            crate::domain::audit::AuditKind::StatusChanged {
                from,
                to,
                rejection_reason,
                ..
            } => {
                assert_eq!(from, ProposalStatus::Submitted);
                assert_eq!(to, ProposalStatus::Rejected);
                assert_eq!(rejection_reason, Some(reason));
            }
            other => panic!("expected StatusChanged, got {other:?}"),
        }
    }

    #[ignore]
    #[tokio::test]
    async fn accept_verdict_writes_the_simulation_outcome() {
        let (store, mut audit) = test_store().await;
        let id = store
            .insert(test_proposal(
                test_order_uid(),
                SOLVER_A,
                ProposalStatus::Submitted,
            ))
            .await
            .expect("insert");
        let _received = audit.try_recv().expect("insert event");

        let sim = SimulationOutcome {
            gas_used: 200_000,
            trampoline: address!("00000000000000000000000000000000000000ee"),
            sell_token: address!("00000000000000000000000000000000000000cc"),
            buy_token: address!("00000000000000000000000000000000000000dd"),
        };
        let status = store
            .resolve_verdict(id, Verdict::Accept(Some(sim)))
            .await
            .expect("verdict lands");
        assert_eq!(status, ProposalStatus::Active);

        let fetched = store.get(id).await.expect("get").expect("exists");
        assert_eq!(fetched.status, ProposalStatus::Active);
        assert_eq!(fetched.gas_used, Some(200_000));
        assert_eq!(fetched.trampoline, Some(sim.trampoline));
        assert_eq!(fetched.sell_token, sim.sell_token);
        assert_eq!(fetched.buy_token, sim.buy_token);

        let event = audit.try_recv().expect("activation should emit an event");
        assert_eq!(event.event_type(), "validated");
    }

    #[ignore]
    #[tokio::test]
    async fn revalidation_of_active_updates_gas_without_an_event() {
        let (store, mut audit) = test_store().await;
        let mut proposal = make_proposal(test_order_uid(), SOLVER_A);
        proposal.gas_used = Some(100_000);
        let id = store.insert(proposal).await.expect("insert");
        let _received = audit.try_recv().expect("insert event");

        let sim = SimulationOutcome {
            gas_used: 150_000,
            trampoline: address!("00000000000000000000000000000000000000ee"),
            sell_token: address!("00000000000000000000000000000000000000cc"),
            buy_token: address!("00000000000000000000000000000000000000dd"),
        };
        let status = store
            .resolve_verdict(id, Verdict::Accept(Some(sim)))
            .await
            .expect("verdict lands");
        assert_eq!(status, ProposalStatus::Active);

        let fetched = store.get(id).await.expect("get").expect("exists");
        assert_eq!(
            fetched.gas_used,
            Some(150_000),
            "re-validation refreshes gas"
        );
        assert!(
            audit.try_recv().is_err(),
            "Active → Active must not emit an audit event"
        );
    }

    #[ignore]
    #[tokio::test]
    async fn verdict_on_terminal_proposal_is_stale() {
        let (store, mut audit) = test_store().await;
        let id = store
            .insert(test_proposal(
                test_order_uid(),
                SOLVER_A,
                ProposalStatus::Cancelled,
            ))
            .await
            .expect("insert");
        let _received = audit.try_recv().expect("insert event");

        let err = store
            .resolve_verdict(id, Verdict::Accept(None))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::StaleTransition { .. }));
        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Cancelled,
            "a stale Accept verdict must not resurrect a cancelled proposal"
        );
        assert!(
            audit.try_recv().is_err(),
            "stale verdicts leave no evidence"
        );
    }

    #[ignore]
    #[tokio::test]
    async fn transition_nonexistent_fails_with_not_found() {
        let (store, _audit) = test_store().await;
        let err = store
            .transition(
                ProposalId(999),
                ProposalStatus::Active,
                ProposalStatus::Expired,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    #[ignore]
    #[tokio::test]
    async fn cancel_sets_status_and_emits_event() {
        let (store, mut audit) = test_store().await;
        let id = store
            .insert(make_proposal(test_order_uid(), SOLVER_A))
            .await
            .expect("insert");
        let _received = audit.try_recv().expect("insert event");

        store.cancel(id, SOLVER_A).await.expect("cancel succeeds");

        let fetched = store.get(id).await.expect("get").expect("exists");
        assert_eq!(fetched.status, ProposalStatus::Cancelled);

        let event = audit.try_recv().expect("cancel should emit an audit event");
        assert_eq!(event.proposal_id(), id);
        assert_eq!(event.sub_solver(), SOLVER_A);
        assert_eq!(*event.order_uid(), test_order_uid());
        assert!(matches!(
            event.kind,
            crate::domain::audit::AuditKind::Cancelled { .. }
        ));
    }

    #[ignore]
    #[tokio::test]
    async fn cancel_wrong_owner_fails_without_evidence() {
        let (store, mut audit) = test_store().await;
        let id = store
            .insert(make_proposal(test_order_uid(), SOLVER_A))
            .await
            .expect("insert");
        let _received = audit.try_recv().expect("insert event");

        let err = store.cancel(id, SOLVER_B).await.unwrap_err();
        assert!(matches!(err, StoreError::NotOwner(_, _)));
        assert!(
            audit.try_recv().is_err(),
            "failed cancel must not leave a cancelled event"
        );
    }

    #[ignore]
    #[tokio::test]
    async fn cancel_terminal_state_fails() {
        let (store, _audit) = test_store().await;
        let id = store
            .insert(test_proposal(
                test_order_uid(),
                SOLVER_A,
                ProposalStatus::Settled,
            ))
            .await
            .expect("insert");

        let err = store.cancel(id, SOLVER_A).await.unwrap_err();
        assert!(matches!(err, StoreError::StaleTransition { .. }));
        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Settled,
            "a settled proposal must stay settled"
        );
    }

    #[ignore]
    #[tokio::test]
    async fn cancel_submitted_proposal_succeeds() {
        let (store, _audit) = test_store().await;
        let id = store
            .insert(test_proposal(
                test_order_uid(),
                SOLVER_A,
                ProposalStatus::Submitted,
            ))
            .await
            .expect("insert");

        store
            .cancel(id, SOLVER_A)
            .await
            .expect("cancel before verdict");
        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Cancelled
        );
    }

    #[ignore]
    #[tokio::test]
    async fn cancel_nonexistent_fails() {
        let (store, _audit) = test_store().await;
        let err = store.cancel(ProposalId(999), SOLVER_A).await.unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    #[ignore]
    #[tokio::test]
    async fn list_by_order_uid_returns_only_active_proposals_on_that_order() {
        let (store, _audit) = test_store().await;
        let uid = test_order_uid();
        store
            .insert(make_proposal(uid.clone(), SOLVER_A))
            .await
            .expect("insert");
        store
            .insert(make_proposal(uid.clone(), SOLVER_A))
            .await
            .expect("insert");
        // Submitted on the same order: not yet gatekept, must not appear.
        store
            .insert(test_proposal(
                uid.clone(),
                SOLVER_A,
                ProposalStatus::Submitted,
            ))
            .await
            .expect("insert");
        // Active on a different order: must not appear.
        store
            .insert(make_proposal(OrderUid([0xbb; 56]), SOLVER_A))
            .await
            .expect("insert");

        let results = store.list_by_order_uid(&uid).await.expect("list");
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|p| p.status == ProposalStatus::Active));
    }

    #[ignore]
    #[tokio::test]
    async fn list_by_sub_solver_shows_live_proposals_of_that_owner_only() {
        let (store, _audit) = test_store().await;
        store
            .insert(test_proposal(
                test_order_uid(),
                SOLVER_A,
                ProposalStatus::Submitted,
            ))
            .await
            .expect("insert");
        store
            .insert(make_proposal(OrderUid([0xbb; 56]), SOLVER_A))
            .await
            .expect("insert");
        store
            .insert(test_proposal(
                OrderUid([0xcc; 56]),
                SOLVER_A,
                ProposalStatus::Rejected,
            ))
            .await
            .expect("insert");
        store
            .insert(make_proposal(test_order_uid(), SOLVER_B))
            .await
            .expect("insert");

        let results = store.list_by_sub_solver(SOLVER_A).await.expect("list");
        assert_eq!(results.len(), 2, "submitted + active, not the rejected one");
        assert!(results.iter().all(|p| p.sub_solver == SOLVER_A));
    }

    #[ignore]
    #[tokio::test]
    async fn cancelled_proposals_disappear_from_the_lists() {
        let (store, _audit) = test_store().await;
        let uid = test_order_uid();
        let id = store
            .insert(make_proposal(uid.clone(), SOLVER_A))
            .await
            .expect("insert");
        store.cancel(id, SOLVER_A).await.expect("cancel");

        assert!(
            store
                .list_by_order_uid(&uid)
                .await
                .expect("list")
                .is_empty()
        );
        assert!(
            store
                .list_by_sub_solver(SOLVER_A)
                .await
                .expect("list")
                .is_empty()
        );
    }

    #[ignore]
    #[tokio::test]
    async fn snapshot_by_statuses_returns_the_matching_rows() {
        let (store, _audit) = test_store().await;
        store
            .insert(test_proposal(
                test_order_uid(),
                SOLVER_A,
                ProposalStatus::Submitted,
            ))
            .await
            .expect("insert");
        store
            .insert(make_proposal(OrderUid([0xbb; 56]), SOLVER_A))
            .await
            .expect("insert");
        store
            .insert(test_proposal(
                OrderUid([0xcc; 56]),
                SOLVER_A,
                ProposalStatus::Expired,
            ))
            .await
            .expect("insert");

        let live = store
            .snapshot_by_statuses(&[ProposalStatus::Submitted, ProposalStatus::Active])
            .await
            .expect("snapshot");
        assert_eq!(live.len(), 2);
    }

    /// The owner cancels after the validator snapshotted the proposal but
    /// before the verdict lands: applying the verdict must fail and the
    /// cancellation must stick (COW-1202 acceptance criterion).
    #[ignore]
    #[tokio::test]
    async fn cancellation_during_validation_wins_over_the_verdict() {
        let (store, _audit) = test_store().await;
        let id = store
            .insert(test_proposal(
                test_order_uid(),
                SOLVER_A,
                ProposalStatus::Submitted,
            ))
            .await
            .expect("insert");

        store.cancel(id, SOLVER_A).await.expect("cancel succeeds");
        let stale = store.resolve_verdict(id, Verdict::Accept(None)).await;

        assert!(stale.is_err(), "stale verdict must be dropped");
        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Cancelled,
            "a stale Accept verdict must not resurrect a cancelled proposal"
        );
    }
}
