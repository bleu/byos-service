//! Postgres-backed proposal store (ADR-0013): the `proposals` table is the
//! single source of truth for current state. Ingestion, reads, cancellation,
//! the background validator, and `/solve` all read and write it directly —
//! no cache layer. The table holds what *is*; `audit_events` holds what
//! *happened*.
//!
//! Every status change emits an [`audit::AuditEvent`] — auditing happens by
//! construction, so future transition sites cannot forget to leave evidence.
//! The one mutation without its own event is the `penalties` queue insert,
//! which rides along inside the transaction that emits the accompanying
//! `StatusChanged`.

use {
    crate::domain::{
        audit,
        proposal::{OrderUid, Proposal, ProposalId, ProposalStatus, SettlementOutcome},
        validator::RejectionReason,
    },
    alloy::primitives::{Address, B256, Bytes, U256},
    byos_common::contracts::Interaction,
    sqlx::postgres::PgPool,
    std::{
        sync::Arc,
        time::{Duration, SystemTime},
    },
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
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A stored row cannot be read back into a domain type. Split from
    /// `Database` because the two want opposite handling: a connection blip is
    /// worth retrying next tick, a row that will never parse is not, and
    /// packing both into `Database` meant callers retried the unparseable one
    /// forever with no way to tell them apart except by matching on the
    /// message (ADR-0007: split by what the caller should do).
    #[error("corrupt {table}.{column}: {detail}")]
    CorruptRow {
        /// Named explicitly: the message used to hard-code `proposals`, which
        /// sent an operator to the wrong table for the `penalties` reads.
        table: &'static str,
        column: &'static str,
        detail: String,
    },
}

impl StoreError {
    /// Whether retrying this call could plausibly succeed.
    ///
    /// `false` means the data or the schema is the problem, so a caller should
    /// surface it rather than spend a tick on it every pass. A bad migration
    /// lands in `Database` rather than `CorruptRow` — a missing column is
    /// likelier than an unparseable value the service itself wrote — so the
    /// sqlx variants that can never resolve on their own are classified here
    /// too.
    pub fn should_retry(&self) -> bool {
        match self {
            // Neither retryable nor a data fault: the caller lost a race, or
            // asked for something that is not there.
            Self::NotFound(_) | Self::NotOwner(_, _) | Self::StaleTransition { .. } => false,
            Self::CorruptRow { .. } => false,
            Self::Database(e) => !matches!(
                e,
                sqlx::Error::Configuration(_)
                    | sqlx::Error::Decode(_)
                    | sqlx::Error::Encode(_)
                    | sqlx::Error::ColumnNotFound(_)
                    | sqlx::Error::ColumnIndexOutOfBounds { .. }
                    | sqlx::Error::ColumnDecode { .. }
                    | sqlx::Error::TypeNotFound { .. }
                    | sqlx::Error::Migrate(_)
            ),
        }
    }
}

/// What [`ProposalStore::apply_settlement_outcome`] did, for the caller's log.
#[derive(Debug, PartialEq, Eq)]
pub enum OutcomeEffect {
    Applied {
        from: ProposalStatus,
        to: ProposalStatus,
    },
    /// The outcome is not legal from the status the row actually held.
    Ignored { from: ProposalStatus },
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
             nonce, signature, status, rejection_reason, gas_used, trampoline, \
             settlement_tx_hash, penalty_tx_hash) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, \
             $10, $11, $12, $13, $14, $15, $16, $17, $18) RETURNING id",
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
        .bind(proposal.settlement_tx_hash.map(|t| format!("{t:#x}")))
        .bind(proposal.penalty_tx_hash.map(|t| format!("{t:#x}")))
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

    /// Transition a proposal from its snapshot status to `to`, only if the
    /// row is still in that status — one compare-and-swap `UPDATE`. Zero
    /// rows affected means someone else (e.g. a cancellation) won the race:
    /// the caller's write is stale and must be dropped. The audit event is
    /// built from the snapshot's identity fields (immutable after insert),
    /// so a committed transition can never fail to produce its evidence.
    pub async fn transition(
        &self,
        proposal: &Proposal,
        to: ProposalStatus,
    ) -> Result<(), StoreError> {
        let from = proposal.status;
        let result = sqlx::query(
            "UPDATE proposals SET status = $3, status_changed_at = now() WHERE id = $1 AND status \
             = $2",
        )
        .bind(as_db_id(proposal.id)?)
        .bind(from.to_string())
        .bind(to.to_string())
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(self.stale_or_missing(proposal.id, from.to_string()).await);
        }

        self.emit(audit::AuditEvent {
            occurred_at: SystemTime::now(),
            kind: audit::AuditKind::StatusChanged {
                proposal_id: proposal.id,
                sub_solver: proposal.sub_solver,
                order_uid: proposal.order_uid.clone(),
                from,
                to,
                rejection_reason: None,
                settlement_tx_hash: None,
            },
        });
        Ok(())
    }

    /// Record a landed Track A escrow debit (ADR-0003, COW-1205):
    /// `SettleFailed` → `Penalized`, citing the debit transaction and its
    /// amount as evidence. Same compare-and-swap semantics as
    /// [`Self::transition`].
    pub async fn record_penalty(
        &self,
        proposal: &Proposal,
        amount: U256,
        penalty_tx_hash: B256,
    ) -> Result<(), StoreError> {
        let from = proposal.status;
        let result = sqlx::query(
            "UPDATE proposals SET status = $3, penalty_tx_hash = $4, status_changed_at = now() \
             WHERE id = $1 AND status = $2",
        )
        .bind(as_db_id(proposal.id)?)
        .bind(from.to_string())
        .bind(ProposalStatus::Penalized.to_string())
        .bind(format!("{penalty_tx_hash:#x}"))
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(self.stale_or_missing(proposal.id, from.to_string()).await);
        }

        self.emit(audit::AuditEvent {
            occurred_at: SystemTime::now(),
            kind: audit::AuditKind::Penalized {
                proposal_id: proposal.id,
                sub_solver: proposal.sub_solver,
                order_uid: proposal.order_uid.clone(),
                amount,
                settlement_tx_hash: proposal.settlement_tx_hash,
                penalty_tx_hash,
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
        .bind(as_db_id(id)?)
        .fetch_optional(&mut *tx)
        .await?;

        let Some((status, sub_solver, order_uid)) = row else {
            return Err(StoreError::NotFound(id));
        };
        // Parse everything the audit event needs before writing: a corrupt
        // row must fail here, not after the commit — a committed mutation
        // without its evidence is the one state the audit design forbids.
        let from: ProposalStatus = status.parse().map_err(|e| corrupt("status", e))?;
        let sub_solver: Address = sub_solver.parse().map_err(|e| corrupt("sub_solver", e))?;
        let order_uid: OrderUid = order_uid.parse().map_err(|e| corrupt("order_uid", e))?;
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
        .bind(as_db_id(id)?)
        .bind(to.to_string())
        .bind(rejection_reason.map(|r| r.to_string()))
        // Saturating, not `expect`: the validator already defers on a gas
        // value this column cannot hold, so this is a backstop that must not
        // be able to panic the validation loop. A saturated value scores the
        // proposal out of contention, which is the safe direction.
        .bind(sim.map(|s| i64::try_from(s.gas_used).unwrap_or(i64::MAX)))
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
                    sub_solver,
                    order_uid,
                    from,
                    to,
                    rejection_reason,
                    settlement_tx_hash: None,
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
        .bind(as_db_id(id)?)
        .fetch_optional(&mut *tx)
        .await?;

        let Some((status, owner, order_uid)) = row else {
            return Err(StoreError::NotFound(id));
        };
        // Parse before writing (see resolve_verdict); comparing owners as
        // `Address` values also avoids coupling the check to the exact hex
        // formatting the insert used.
        let owner: Address = owner.parse().map_err(|e| corrupt("sub_solver", e))?;
        let order_uid: OrderUid = order_uid.parse().map_err(|e| corrupt("order_uid", e))?;
        if owner != sub_solver {
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
            .bind(as_db_id(id)?)
            .bind(ProposalStatus::Cancelled.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        self.emit(audit::AuditEvent {
            occurred_at: SystemTime::now(),
            kind: audit::AuditKind::Cancelled {
                proposal_id: id,
                sub_solver,
                order_uid,
            },
        });
        Ok(())
    }

    /// Apply a driver-reported settlement outcome (ADR-0010, ADR-0013),
    /// deciding the transition from the row as it stands under the lock.
    ///
    /// The status a given outcome is legal from is only knowable at the moment
    /// of the write: the driver does not order its notifications, and the
    /// executing timeout, a cancellation, and the expiry sweep all move the
    /// row underneath. Deciding from a snapshot read outside the transaction
    /// means a `Revert` that arrives while `settlementStarted` is still
    /// committing looks illegal and gets dropped — and a dropped `Revert` is a
    /// Track A debit the sub-solver never pays.
    ///
    /// Returns what happened so the caller can log it; an outcome that is
    /// genuinely illegal from the committed status is `Ok(Ignored)`, not an
    /// error, because the timeout backstop and re-simulation reconcile it.
    pub async fn apply_settlement_outcome(
        &self,
        proposal: &Proposal,
        outcome: SettlementOutcome,
    ) -> Result<OutcomeEffect, StoreError> {
        let id = proposal.id;
        let mut tx = self.pool.begin().await?;
        let row: Option<(String,)> =
            sqlx::query_as("SELECT status FROM proposals WHERE id = $1 FOR UPDATE")
                .bind(as_db_id(id)?)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((status,)) = row else {
            return Err(StoreError::NotFound(id));
        };
        let from: ProposalStatus = status.parse().map_err(|e| corrupt("status", e))?;

        // The legality table.
        //
        // The two outcomes that cite a transaction are legal from `Active` as
        // well as `Executing`, and that is the point of this function. A tx
        // hash is an on-chain fact, and `attributed_proposals` has already
        // proved via the `solutions` join that we bid this proposal into that
        // auction — so `Active` means we lost track of the submission, not
        // that the settlement never happened. Requiring `Executing` forfeits
        // the Track A debit in every case where `settlementStarted` did not
        // commit: the driver never sent it, its write failed, the process
        // restarted in between, or the two notifications simply arrived out of
        // order.
        //
        // `Started` stays `Active`-only: nothing to reconcile, and it must not
        // drag a terminal proposal back into flight.
        //
        // `Abandoned` stays `Executing`-only, deliberately. It cites no
        // transaction, and the driver's `fail` covers more than "abandoned
        // after submission", so from `Active` there is nothing to release and
        // no evidence that a charge is owed.
        let to = match (outcome, from) {
            (SettlementOutcome::Started, ProposalStatus::Active) => ProposalStatus::Executing,
            (
                SettlementOutcome::Succeeded(_),
                ProposalStatus::Executing | ProposalStatus::Active,
            ) => ProposalStatus::Settled,
            (
                SettlementOutcome::Reverted(_),
                ProposalStatus::Executing | ProposalStatus::Active,
            ) => ProposalStatus::SettleFailed,
            (SettlementOutcome::Abandoned, ProposalStatus::Executing) => ProposalStatus::Active,
            _ => return Ok(OutcomeEffect::Ignored { from }),
        };

        let settlement_tx = match outcome {
            SettlementOutcome::Succeeded(tx_hash) | SettlementOutcome::Reverted(tx_hash) => {
                Some(tx_hash)
            }
            _ => None,
        };
        sqlx::query(
            "UPDATE proposals SET status = $2, settlement_tx_hash = COALESCE($3, \
             settlement_tx_hash), status_changed_at = now() WHERE id = $1",
        )
        .bind(as_db_id(id)?)
        .bind(to.to_string())
        .bind(settlement_tx.map(|t| format!("{t:#x}")))
        .execute(&mut *tx)
        .await?;

        // Queued inside the same transaction as the release, so the charge
        // cannot be lost to a crash between the two.
        //
        // The only guard against a duplicate charge is the status CAS above: a
        // repeat notification finds the row `Active` and is ignored. That is
        // keyed on status alone, not on the settlement, so a retransmitted
        // `fail` arriving after the proposal re-won and re-entered `Executing`
        // would queue a second row for one lost settlement. `penalties` has no
        // uniqueness constraint to catch it. Narrow enough to accept; closing
        // it means keying the row on something that identifies the settlement.
        //
        // Liveness cost of the shared transaction: an INSERT failure now rolls
        // the release back too, so the proposal stays `Executing` until
        // `release_stale_executing` picks it up (default 5 minutes) — and that
        // path queues no penalty. Previously the release survived and only the
        // charge was lost. Both under-charge; this one is recoverable by hand.
        if outcome == SettlementOutcome::Abandoned {
            sqlx::query(
                "INSERT INTO penalties (proposal_id, sub_solver, order_uid) VALUES ($1, $2, $3)",
            )
            .bind(as_db_id(id)?)
            .bind(format!("{:#x}", proposal.sub_solver))
            .bind(proposal.order_uid.to_string())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        self.emit(audit::AuditEvent {
            occurred_at: SystemTime::now(),
            kind: audit::AuditKind::StatusChanged {
                proposal_id: id,
                sub_solver: proposal.sub_solver,
                order_uid: proposal.order_uid.clone(),
                from,
                to,
                rejection_reason: None,
                settlement_tx_hash: settlement_tx,
            },
        });
        Ok(OutcomeEffect::Applied { from, to })
    }

    /// Keep an attributable non-outcome driver notification (a
    /// pre-submission kind like `emptySolution`) as audit evidence — no
    /// transition, no row mutation (ADR-0013).
    pub fn note_driver_notification(&self, proposal: &Proposal, kind: &str) {
        self.emit(audit::AuditEvent {
            occurred_at: SystemTime::now(),
            kind: audit::AuditKind::DriverNotified {
                proposal_id: proposal.id,
                sub_solver: proposal.sub_solver,
                order_uid: proposal.order_uid.clone(),
                kind: kind.to_owned(),
            },
        });
    }

    /// Release `Executing` proposals older than `older_than` back to
    /// `Active` (ADR-0013's timeout backstop: lost notification, restart
    /// mid-settlement); returns how many were released. Always safe — if
    /// the settlement actually landed, the next re-simulation reverts and
    /// the proposal dies as `SimFailed`.
    pub async fn release_stale_executing(&self, older_than: Duration) -> Result<u64, StoreError> {
        // Parse inside the transaction (see resolve_verdict): a corrupt row
        // must abort the release, not commit a transition without evidence.
        let mut tx = self.pool.begin().await?;
        let rows: Vec<(i64, String, String)> = sqlx::query_as(
            "UPDATE proposals SET status = $1, status_changed_at = now() WHERE status = $2 AND \
             status_changed_at < now() - make_interval(secs => $3) RETURNING id, sub_solver, \
             order_uid",
        )
        .bind(ProposalStatus::Active.to_string())
        .bind(ProposalStatus::Executing.to_string())
        .bind(older_than.as_secs_f64())
        .fetch_all(&mut *tx)
        .await?;

        let released = rows
            .into_iter()
            .map(|(id, sub_solver, order_uid)| {
                Ok((
                    ProposalId(u64::try_from(id).map_err(|e| corrupt("id", e))?),
                    sub_solver
                        .parse::<Address>()
                        .map_err(|e| corrupt("sub_solver", e))?,
                    order_uid
                        .parse::<OrderUid>()
                        .map_err(|e| corrupt("order_uid", e))?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        tx.commit().await?;

        let count = released.len() as u64;
        for (proposal_id, sub_solver, order_uid) in released {
            tracing::info!(id = %proposal_id, "executing timeout: proposal released to active");
            self.emit(audit::AuditEvent {
                occurred_at: SystemTime::now(),
                kind: audit::AuditKind::StatusChanged {
                    proposal_id,
                    sub_solver,
                    order_uid,
                    from: ProposalStatus::Executing,
                    to: ProposalStatus::Active,
                    rejection_reason: None,
                    settlement_tx_hash: None,
                },
            });
        }
        Ok(count)
    }

    /// Delete dropped-tier proposals (`Rejected`/`SimFailed`/`Expired`/
    /// `Cancelled`) that reached their terminal state more than `older_than`
    /// ago; returns how many were deleted. The money states
    /// (`Settled`/`SettleFailed`/`Penalized`) are never swept, and
    /// `audit_events` is never touched — the proposal's history outlives
    /// its row (ADR-0013).
    pub async fn sweep_dropped(&self, older_than: Duration) -> Result<u64, StoreError> {
        const DROPPED: [ProposalStatus; 4] = [
            ProposalStatus::Rejected,
            ProposalStatus::SimFailed,
            ProposalStatus::Expired,
            ProposalStatus::Cancelled,
        ];
        let result = sqlx::query(
            "DELETE FROM proposals WHERE status = ANY($1) AND status_changed_at < now() - \
             make_interval(secs => $2)",
        )
        .bind(status_names(&DROPPED))
        .bind(older_than.as_secs_f64())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Disambiguate a zero-row compare-and-swap: the proposal is either gone
    /// (`NotFound`) or sits in a different status (`StaleTransition`).
    async fn stale_or_missing(&self, id: ProposalId, expected: String) -> StoreError {
        let db_id = match as_db_id(id) {
            Ok(db_id) => db_id,
            // An id the column cannot hold never named a row, so the
            // zero-row CAS was a miss, not a lost race.
            Err(e) => return e,
        };
        let status: Option<String> =
            match sqlx::query_scalar("SELECT status FROM proposals WHERE id = $1")
                .bind(db_id)
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
        Ok(decode_rows(rows, "list_by_order_uid"))
    }

    /// List one owner's active proposals for a given order UID — the
    /// `GET /proposals/{order_uid}` view. The owner scoping lives in the
    /// query (ADR-0011): competitors' proposals on the same order are
    /// invisible to the caller.
    pub async fn list_by_order_uid_for_owner(
        &self,
        order_uid: &OrderUid,
        owner: Address,
    ) -> Result<Vec<Proposal>, StoreError> {
        let rows: Vec<ProposalRow> = sqlx::query_as(&format!(
            "SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE order_uid = $1 AND sub_solver = $2 \
             AND status = $3 ORDER BY id"
        ))
        .bind(order_uid.to_string())
        .bind(format!("{owner:#x}"))
        .bind(ProposalStatus::Active.to_string())
        .fetch_all(&self.pool)
        .await?;
        Ok(decode_rows(rows, "list_by_order_uid_for_owner"))
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
        Ok(decode_rows(rows, "list_by_sub_solver"))
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
        Ok(decode_rows(rows, "snapshot_by_statuses"))
    }

    /// Queue the 0.1 × c_l non-settlement charge for a proposal whose won
    /// settlement was abandoned (ADR-0003, COW-1205). Called by `/notify`
    /// after the `Executing` → `Active` transition commits — the CAS there
    /// is what makes one lost settlement queue exactly one charge.
    pub async fn queue_non_settlement_penalty(
        &self,
        proposal: &Proposal,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO penalties (proposal_id, sub_solver, order_uid) VALUES ($1, $2, $3)",
        )
        .bind(as_db_id(proposal.id)?)
        .bind(format!("{:#x}", proposal.sub_solver))
        .bind(proposal.order_uid.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every queued non-settlement charge not yet debited — the penalty
    /// loop's per-tick working set.
    pub async fn pending_penalties(
        &self,
    ) -> Result<Vec<crate::domain::penalty::PendingPenalty>, StoreError> {
        let rows: Vec<(i64, i64, String, String)> = sqlx::query_as(
            "SELECT id, proposal_id, sub_solver, order_uid FROM penalties WHERE penalty_tx_hash \
             IS NULL ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(id, proposal_id, sub_solver, order_uid)| {
                Ok(crate::domain::penalty::PendingPenalty {
                    id,
                    proposal_id: ProposalId(
                        u64::try_from(proposal_id)
                            .map_err(|e| corrupt_in("penalties", "proposal_id", e))?,
                    ),
                    sub_solver: sub_solver
                        .parse()
                        .map_err(|e| corrupt_in("penalties", "sub_solver", e))?,
                    order_uid: order_uid
                        .parse()
                        .map_err(|e| corrupt_in("penalties", "order_uid", e))?,
                })
            })
            .collect()
    }

    /// Record a landed non-settlement debit: fills the `penalties` row's
    /// `penalty_tx_hash` (leaving the pending queue) and emits the charge as
    /// evidence. The proposal row is untouched — it is `Active` again and
    /// still competing.
    pub async fn record_non_settlement_debit(
        &self,
        penalty: &crate::domain::penalty::PendingPenalty,
        amount: U256,
        penalty_tx_hash: B256,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE penalties SET penalty_tx_hash = $2 WHERE id = $1")
            .bind(penalty.id)
            .bind(format!("{penalty_tx_hash:#x}"))
            .execute(&self.pool)
            .await?;

        self.emit(audit::AuditEvent {
            occurred_at: SystemTime::now(),
            kind: audit::AuditKind::NonSettlementDebited {
                proposal_id: penalty.proposal_id,
                sub_solver: penalty.sub_solver,
                order_uid: penalty.order_uid.clone(),
                amount,
                penalty_tx_hash,
            },
        });
        Ok(())
    }

    /// Record which proposal a returned solution was built on — the join key
    /// for driver `/notify` attribution (ADR-0013). `/solve` calls this
    /// before returning the solution: if we can't record it, we don't bid
    /// it. The upsert covers both a re-run auction (driver restart) and a
    /// solution id re-used after a dropped bid in the same response —
    /// either way the stale mapping is overwritten before its solution is
    /// ever returned.
    pub async fn record_solution(
        &self,
        auction_id: i64,
        solution_id: i64,
        proposal_id: ProposalId,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO solutions (auction_id, solution_id, proposal_id) VALUES ($1, $2, $3) ON \
             CONFLICT (auction_id, solution_id) DO UPDATE SET proposal_id = EXCLUDED.proposal_id",
        )
        .bind(auction_id)
        .bind(solution_id)
        .bind(as_db_id(proposal_id)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The proposals a driver notification points at, joined through the
    /// `solutions` table. Notifications carry one solution id or a merged
    /// list; ids we never issued simply match nothing.
    pub async fn solution_proposals(
        &self,
        auction_id: i64,
        solution_ids: &[u64],
    ) -> Result<Vec<Proposal>, StoreError> {
        // Ids beyond i64 cannot be in the table (we issued small ones).
        let ids: Vec<i64> = solution_ids
            .iter()
            .filter_map(|id| i64::try_from(*id).ok())
            .collect();
        let rows: Vec<ProposalRow> = sqlx::query_as(&format!(
            "SELECT {PROPOSAL_COLUMNS} FROM proposals JOIN solutions ON solutions.proposal_id = \
             proposals.id WHERE solutions.auction_id = $1 AND solutions.solution_id = ANY($2) \
             ORDER BY id"
        ))
        .bind(auction_id)
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(decode_rows(rows, "solution_proposals"))
    }

    /// Look up a single proposal by ID.
    ///
    /// An id the `id` column cannot hold is a miss, not an error: the caller
    /// maps `Ok(None)` to the owner-scoped 404 (ADR-0011) and every
    /// `StoreError` to a 500, and an unrepresentable id is the former.
    pub async fn get(&self, id: ProposalId) -> Result<Option<Proposal>, StoreError> {
        let Ok(db_id) = as_db_id(id) else {
            return Ok(None);
        };
        let row: Option<ProposalRow> = sqlx::query_as(&format!(
            "SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE id = $1"
        ))
        .bind(db_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Proposal::try_from).transpose()
    }
}

/// Decode a batch of rows, dropping the ones that cannot be read.
///
/// The rule across the store: a *collection* returns what it can read, a single
/// lookup ([`ProposalStore::get`]) reports that it could not read the row.
/// Collecting into `Result` instead — what every one of these call sites used
/// to do — means one unreadable row fails the whole query, and the blast radius
/// is severe: `snapshot_by_statuses` is the validation loop's working set, so a
/// single bad row stopped every verdict, every expiry, and (via the penalty
/// loop's own snapshot) every Track A debit, on every tick, forever. Skipping
/// costs one proposal; propagating cost the whole book.
///
/// `error!` rather than `warn!` because nothing else will notice: the row keeps
/// its status, no sweep can move it, and it needs a human.
fn decode_rows(rows: Vec<ProposalRow>, context: &str) -> Vec<Proposal> {
    let total = rows.len();
    let decoded: Vec<Proposal> = rows
        .into_iter()
        .filter_map(|row| match Proposal::try_from(row) {
            Ok(proposal) => Some(proposal),
            Err(e) => {
                tracing::error!(%e, context, "unreadable proposal row skipped");
                None
            }
        })
        .collect();
    if decoded.len() != total {
        tracing::error!(
            skipped = total - decoded.len(),
            total,
            context,
            "proposal rows dropped from this read; manual repair needed"
        );
    }
    decoded
}

/// A `ProposalId` as the BIGINT the `id` column holds.
///
/// Ids reach the store from two places: the Postgres sequence, always in
/// range, and `Path<ProposalId>` on the wire, which is a bare `u64` and so can
/// exceed `i64::MAX`. An id the column cannot hold names no row, so it is a
/// miss rather than a panic — the wire path is reachable by anyone who can
/// reach `GET /proposal/{id}`.
fn as_db_id(id: ProposalId) -> Result<i64, StoreError> {
    i64::try_from(id.0).map_err(|_| StoreError::NotFound(id))
}

/// Status strings for a `= ANY($n)` bind.
fn status_names(statuses: &[ProposalStatus]) -> Vec<String> {
    statuses.iter().map(|s| s.to_string()).collect()
}

// ---------------------------------------------------------------------------
// Row codec
// ---------------------------------------------------------------------------

const PROPOSAL_COLUMNS: &str =
    "id, sub_solver, order_uid, order_uid_hash, sell_amount, buy_amount, sell_token, buy_token, \
     interactions, interactions_hash, valid_until, nonce, signature, status, rejection_reason, \
     gas_used, trampoline, settlement_tx_hash, penalty_tx_hash";

/// The raw column values; [`Proposal::try_from`] parses them back into
/// domain types. A parse failure means a corrupt row (we wrote these values),
/// surfaced as [`StoreError::CorruptRow`] — which batch reads skip and single
/// lookups report.
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
    settlement_tx_hash: Option<String>,
    penalty_tx_hash: Option<String>,
}

/// A column in `proposals` that cannot be parsed back into its domain type.
fn corrupt(column: &'static str, err: impl std::fmt::Display) -> StoreError {
    corrupt_in("proposals", column, err)
}

/// As [`corrupt`], for the reads that decode `penalties` rows.
fn corrupt_in(
    table: &'static str,
    column: &'static str,
    err: impl std::fmt::Display,
) -> StoreError {
    StoreError::CorruptRow {
        table,
        column,
        detail: err.to_string(),
    }
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
            settlement_tx_hash: row
                .settlement_tx_hash
                .map(|t| t.parse::<B256>())
                .transpose()
                .map_err(|e| corrupt("settlement_tx_hash", e))?,
            penalty_tx_hash: row
                .penalty_tx_hash
                .map(|t| t.parse::<B256>())
                .transpose()
                .map_err(|e| corrupt("penalty_tx_hash", e))?,
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
        let (store, rx, _pool) = test_store_with_pool().await;
        (store, rx)
    }

    /// Like [`test_store`], with direct pool access for tests that
    /// manipulate rows underneath the store (e.g. backdating
    /// `status_changed_at`).
    async fn test_store_with_pool() -> (ProposalStore, mpsc::UnboundedReceiver<AuditEvent>, PgPool)
    {
        let db = TestDb::create().await;
        let pool = crate::infra::audit::connect_and_migrate(&db.url)
            .await
            .expect("migrations run");
        let (tx, rx) = mpsc::unbounded_channel();
        (ProposalStore::new(pool.clone(), tx), rx, pool)
    }

    /// Make a stored row undecodable, the way a bad migration or hand-edit
    /// would. `sub_solver` is the column of choice because no read filters on
    /// it, so the row is still selected and the failure lands in the decoder.
    async fn corrupt_sub_solver(pool: &PgPool, id: ProposalId) {
        sqlx::query("UPDATE proposals SET sub_solver = 'not-an-address' WHERE id = $1")
            .bind(as_db_id(id).expect("db-assigned id"))
            .execute(pool)
            .await
            .expect("corrupt the row");
    }

    /// Pretend the proposal reached its current status `secs` seconds ago.
    async fn backdate_status_change(pool: &PgPool, id: ProposalId, secs: f64) {
        sqlx::query(
            "UPDATE proposals SET status_changed_at = now() - make_interval(secs => $2) WHERE id \
             = $1",
        )
        .bind(as_db_id(id).expect("db-assigned id"))
        .bind(secs)
        .execute(pool)
        .await
        .expect("backdate");
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

        let proposal = store.get(id).await.expect("get").expect("exists");
        store
            .transition(&proposal, ProposalStatus::Expired)
            .await
            .expect("transition lands");

        let fetched = store.get(id).await.expect("get").expect("exists");
        assert_eq!(fetched.status, ProposalStatus::Expired);

        let event = audit.try_recv().expect("transition should emit an event");
        assert_eq!(event.proposal_id(), id);
        assert_eq!(event.event_type(), "expired");
    }

    /// An unreadable row is its own error, distinguishable from a connection
    /// blip without matching on the message.
    ///
    /// Both used to be `Database`, so a caller that retried transient failures
    /// retried a row that could never parse on every tick, forever.
    ///
    /// `sub_solver` is corrupted rather than `status` on purpose: the reads
    /// below filter on `status`, so a bad status is excluded by the query and
    /// never reaches the decoder at all.
    #[ignore]
    #[tokio::test]
    async fn an_unparseable_row_is_reported_as_permanent_not_transient() {
        let (store, _audit, pool) = test_store_with_pool().await;
        let id = store
            .insert(make_proposal(test_order_uid(), SOLVER_A))
            .await
            .expect("insert");
        corrupt_sub_solver(&pool, id).await;

        // A single lookup reports it: there is nothing to skip to, and
        // `Ok(None)` would claim the row does not exist.
        let err = store.get(id).await.expect_err("row cannot be read back");
        assert!(
            matches!(
                err,
                StoreError::CorruptRow {
                    table: "proposals",
                    column: "sub_solver",
                    ..
                }
            ),
            "expected a CorruptRow naming the table and column, got {err:?}"
        );
        assert!(
            !err.should_retry(),
            "retrying an unparseable row can never succeed"
        );
    }

    /// One unreadable row must not take down the reads the loops depend on.
    ///
    /// `snapshot_by_statuses` is the validation loop's working set and, through
    /// the penalty loop's own snapshot, the Track A debit queue. Collecting
    /// into `Result` meant a single bad row failed the whole query, so
    /// every verdict, every expiry sweep, and every debit stopped on every
    /// tick, forever, until someone found the row by hand.
    #[ignore]
    #[tokio::test]
    async fn an_unparseable_row_does_not_take_the_whole_snapshot_with_it() {
        let (store, _audit, pool) = test_store_with_pool().await;
        let broken = store
            .insert(make_proposal(test_order_uid(), SOLVER_A))
            .await
            .expect("insert");
        let healthy = store
            .insert(make_proposal(OrderUid([0xbb; 56]), SOLVER_B))
            .await
            .expect("insert");
        corrupt_sub_solver(&pool, broken).await;

        let live = store
            .snapshot_by_statuses(&[ProposalStatus::Active])
            .await
            .expect("a bad row must not fail the snapshot");

        assert_eq!(
            live.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![healthy],
            "the readable proposal still gets its tick; only the bad row drops out"
        );
    }

    /// The outcome is decided from the locked row, not from the caller's copy.
    ///
    /// `/notify` reads the proposal, then applies an outcome; between those
    /// two the driver's own next notification, the executing timeout, or a
    /// cancellation can move the row. Deciding from the caller's copy dropped
    /// a `revert` whose proposal had since entered `Executing`, and a dropped
    /// revert is a Track A debit the sub-solver never pays. The stale copy is
    /// injected directly here — that is exactly what a concurrent handler
    /// holds.
    #[ignore]
    #[tokio::test]
    async fn a_settlement_outcome_is_judged_against_the_committed_status() {
        let (store, _audit) = test_store().await;
        let id = store
            .insert(test_proposal(
                test_order_uid(),
                SOLVER_A,
                ProposalStatus::Active,
            ))
            .await
            .expect("insert");
        let stale = store.get(id).await.expect("get").expect("exists");
        assert_eq!(stale.status, ProposalStatus::Active);

        // The row moves on, as another notification would move it.
        store
            .transition(&stale, ProposalStatus::Executing)
            .await
            .expect("enter executing");

        let tx = alloy::primitives::b256!(
            "3333333333333333333333333333333333333333333333333333333333333333"
        );
        let effect = store
            .apply_settlement_outcome(&stale, SettlementOutcome::Reverted(tx))
            .await
            .expect("store write");

        assert_eq!(
            effect,
            OutcomeEffect::Applied {
                from: ProposalStatus::Executing,
                to: ProposalStatus::SettleFailed
            },
            "the revert is legal from the committed status, whatever the caller's copy said"
        );
        let stored = store.get(id).await.expect("get").expect("exists");
        assert_eq!(stored.status, ProposalStatus::SettleFailed);
        assert_eq!(
            stored.settlement_tx_hash,
            Some(tx),
            "the reverted tx is the evidence the debit cites"
        );
    }

    /// A revert is still charged when `settlementStarted` never landed.
    ///
    /// This is the reachable half of the bug, and the one the first attempt at
    /// this fix missed: requiring `Executing` forfeits the Track A debit
    /// whenever the first notification did not commit — the driver never sent
    /// it, its write failed, the process restarted in between, or the two
    /// simply arrived out of order. A tx hash is an on-chain fact and the
    /// `solutions` join already proved we bid this proposal, so `Active` means
    /// we lost track of the submission, not that it never happened.
    #[ignore]
    #[tokio::test]
    async fn a_revert_without_a_preceding_settlement_started_is_still_charged() {
        let (store, _audit) = test_store().await;
        let id = store
            .insert(test_proposal(
                test_order_uid(),
                SOLVER_A,
                ProposalStatus::Active,
            ))
            .await
            .expect("insert");
        let proposal = store.get(id).await.expect("get").expect("exists");

        let tx = alloy::primitives::b256!(
            "4444444444444444444444444444444444444444444444444444444444444444"
        );
        let effect = store
            .apply_settlement_outcome(&proposal, SettlementOutcome::Reverted(tx))
            .await
            .expect("store write");

        assert_eq!(
            effect,
            OutcomeEffect::Applied {
                from: ProposalStatus::Active,
                to: ProposalStatus::SettleFailed
            },
            "dropping this revert would forfeit the debit entirely"
        );
        let stored = store.get(id).await.expect("get").expect("exists");
        assert_eq!(stored.status, ProposalStatus::SettleFailed);
        assert_eq!(
            stored.settlement_tx_hash,
            Some(tx),
            "the penalty loop prices the debit off this tx"
        );
    }

    /// The other half of the contract: an outcome that is genuinely illegal
    /// from the committed status is reported, not written and not an error.
    #[ignore]
    #[tokio::test]
    async fn an_outcome_illegal_from_the_committed_status_is_ignored() {
        let (store, _audit) = test_store().await;
        let id = store
            .insert(test_proposal(
                test_order_uid(),
                SOLVER_A,
                ProposalStatus::Cancelled,
            ))
            .await
            .expect("insert");
        let proposal = store.get(id).await.expect("get").expect("exists");

        let effect = store
            .apply_settlement_outcome(&proposal, SettlementOutcome::Started)
            .await
            .expect("an inapplicable outcome is not an error");

        assert_eq!(
            effect,
            OutcomeEffect::Ignored {
                from: ProposalStatus::Cancelled
            }
        );
        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Cancelled,
            "a cancelled proposal must not be dragged into Executing"
        );
    }

    /// Acceptance (COW-1205): the audit trail records the debit with its
    /// amount and both transaction hashes — the dispute evidence for a
    /// Track A charge.
    #[ignore]
    #[tokio::test]
    async fn record_penalty_emits_the_debit_evidence() {
        let (store, mut audit) = test_store().await;
        let settlement_tx = alloy::primitives::b256!(
            "2222222222222222222222222222222222222222222222222222222222222222"
        );
        let penalty_tx = alloy::primitives::b256!(
            "7777777777777777777777777777777777777777777777777777777777777777"
        );
        let mut proposal = test_proposal(test_order_uid(), SOLVER_A, ProposalStatus::SettleFailed);
        proposal.settlement_tx_hash = Some(settlement_tx);
        let id = store.insert(proposal).await.expect("insert");
        let _received = audit.try_recv().expect("insert event");

        let stored = store.get(id).await.expect("get").expect("exists");
        store
            .record_penalty(
                &stored,
                alloy::primitives::U256::from(16_000_000_000_000_000u64),
                penalty_tx,
            )
            .await
            .expect("debit landed");

        let event = audit.try_recv().expect("penalty should emit an event");
        assert_eq!(event.proposal_id(), id);
        assert_eq!(event.sub_solver(), SOLVER_A);
        assert_eq!(event.event_type(), "penalized");
        assert_eq!(
            event.settlement_tx_hash(),
            Some(settlement_tx),
            "the indexed evidence column cites the reverted settlement (Track B attribution)"
        );
        let payload = event.payload();
        assert_eq!(payload["amount"], "16000000000000000");
        assert_eq!(payload["penaltyTxHash"], format!("{penalty_tx:#x}"));
        assert_eq!(payload["settlementTxHash"], format!("{settlement_tx:#x}"));
    }

    /// Acceptance (COW-1205): the non-settlement charge is audited with its
    /// amount and debit tx, attributed to the proposal and sub-solver.
    #[ignore]
    #[tokio::test]
    async fn record_non_settlement_debit_emits_the_charge_evidence() {
        let (store, mut audit) = test_store().await;
        let penalty_tx = alloy::primitives::b256!(
            "7777777777777777777777777777777777777777777777777777777777777777"
        );
        let id = store
            .insert(make_proposal(test_order_uid(), SOLVER_A))
            .await
            .expect("insert");
        let _received = audit.try_recv().expect("insert event");
        let stored = store.get(id).await.expect("get").expect("exists");
        store
            .queue_non_settlement_penalty(&stored)
            .await
            .expect("queue");
        let pending = store.pending_penalties().await.expect("pending");

        store
            .record_non_settlement_debit(
                &pending[0],
                alloy::primitives::U256::from(1_000_000_000_000_000u64),
                penalty_tx,
            )
            .await
            .expect("debit landed");

        let event = audit.try_recv().expect("debit should emit an event");
        assert_eq!(event.proposal_id(), id);
        assert_eq!(event.sub_solver(), SOLVER_A);
        assert_eq!(*event.order_uid(), test_order_uid());
        assert_eq!(event.event_type(), "non_settlement_debited");
        let payload = event.payload();
        assert_eq!(payload["amount"], "1000000000000000");
        assert_eq!(payload["penaltyTxHash"], format!("{penalty_tx:#x}"));
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

        // The row is Active; a snapshot claiming Submitted is stale.
        let mut proposal = store.get(id).await.expect("get").expect("exists");
        proposal.status = ProposalStatus::Submitted;
        let err = store
            .transition(&proposal, ProposalStatus::Expired)
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
        let mut proposal = make_proposal(test_order_uid(), SOLVER_A);
        proposal.id = ProposalId(999);
        let err = store
            .transition(&proposal, ProposalStatus::Expired)
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

    /// Attributable non-outcome notifications carry no transition but are
    /// kept as evidence of the driver's view of our solution (ADR-0013).
    #[ignore]
    #[tokio::test]
    async fn driver_notification_note_leaves_evidence_without_touching_the_row() {
        let (store, mut audit) = test_store().await;
        let id = store
            .insert(make_proposal(test_order_uid(), SOLVER_A))
            .await
            .expect("insert");
        let _received = audit.try_recv().expect("insert event");
        let proposal = store.get(id).await.expect("get").expect("exists");

        store.note_driver_notification(&proposal, "emptySolution");

        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Active,
            "a note is evidence, not a transition"
        );
        let event = audit.try_recv().expect("the note must leave evidence");
        assert_eq!(event.proposal_id(), id);
        assert_eq!(event.event_type(), "driver_notified");
        assert_eq!(event.payload()["kind"], "emptySolution");
    }

    /// Acceptance (COW-1204): an `Executing` proposal older than the
    /// executing timeout falls back to `Active` without a notification —
    /// the backstop for lost notifications and restarts mid-settlement.
    #[ignore]
    #[tokio::test]
    async fn stale_executing_proposals_fall_back_to_active() {
        let (store, mut audit, pool) = test_store_with_pool().await;
        let id = store
            .insert(test_proposal(
                test_order_uid(),
                SOLVER_A,
                ProposalStatus::Executing,
            ))
            .await
            .expect("insert");
        let _received = audit.try_recv().expect("insert event");
        backdate_status_change(&pool, id, 400.0).await;

        let released = store
            .release_stale_executing(Duration::from_secs(300))
            .await
            .expect("release");

        assert_eq!(released, 1);
        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Active
        );
        let event = audit.try_recv().expect("a release leaves evidence");
        assert_eq!(event.proposal_id(), id);
        assert_eq!(event.event_type(), "released");
    }

    #[ignore]
    #[tokio::test]
    async fn executing_proposals_inside_the_timeout_are_left_alone() {
        let (store, mut audit) = test_store().await;
        let id = store
            .insert(test_proposal(
                test_order_uid(),
                SOLVER_A,
                ProposalStatus::Executing,
            ))
            .await
            .expect("insert");
        let _received = audit.try_recv().expect("insert event");

        let released = store
            .release_stale_executing(Duration::from_secs(300))
            .await
            .expect("release");

        assert_eq!(released, 0);
        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Executing
        );
        assert!(audit.try_recv().is_err(), "nothing released, no evidence");
    }

    #[ignore]
    #[tokio::test]
    async fn sweep_deletes_dropped_proposals_past_the_window() {
        let (store, _audit, pool) = test_store_with_pool().await;
        // Every dropped-tier status, not just one: the money-state test
        // already loops over all six it must spare, and asserting only
        // `Cancelled` here meant a status quietly dropped from `DROPPED` would
        // leave the table growing without failing anything — the COW-1177 leak
        // ADR-0013 set out to close.
        let mut ids = Vec::new();
        for (i, status) in [
            ProposalStatus::Rejected,
            ProposalStatus::SimFailed,
            ProposalStatus::Expired,
            ProposalStatus::Cancelled,
        ]
        .into_iter()
        .enumerate()
        {
            let uid = OrderUid([u8::try_from(i).expect("few statuses"); 56]);
            let id = store
                .insert(test_proposal(uid, SOLVER_A, status))
                .await
                .expect("insert");
            backdate_status_change(&pool, id, 7200.0).await;
            ids.push((status, id));
        }

        let deleted = store
            .sweep_dropped(Duration::from_secs(3600))
            .await
            .expect("sweep");

        assert_eq!(deleted, 4, "every dropped-tier status is swept");
        for (status, id) in ids {
            assert!(
                store.get(id).await.expect("get").is_none(),
                "a swept {status} proposal reads as gone (404 on the wire)"
            );
        }
    }

    #[ignore]
    #[tokio::test]
    async fn sweep_spares_fresh_dropped_rows() {
        let (store, _audit) = test_store().await;
        let id = store
            .insert(test_proposal(
                test_order_uid(),
                SOLVER_A,
                ProposalStatus::Rejected,
            ))
            .await
            .expect("insert");

        let deleted = store
            .sweep_dropped(Duration::from_secs(3600))
            .await
            .expect("sweep");

        assert_eq!(deleted, 0, "still inside the retention window");
        assert!(store.get(id).await.expect("get").is_some());
    }

    /// The money states (`Settled`/`SettleFailed`/`Penalized`) are never
    /// swept (ADR-0013, COW-1204), and neither are live or in-flight
    /// proposals.
    #[ignore]
    #[tokio::test]
    async fn sweep_never_touches_money_states_or_live_proposals() {
        let (store, _audit, pool) = test_store_with_pool().await;
        let mut spared = Vec::new();
        for (i, status) in [
            ProposalStatus::Settled,
            ProposalStatus::SettleFailed,
            ProposalStatus::Penalized,
            ProposalStatus::Executing,
            ProposalStatus::Active,
            ProposalStatus::Submitted,
        ]
        .into_iter()
        .enumerate()
        {
            let id = store
                .insert(test_proposal(OrderUid([i as u8; 56]), SOLVER_A, status))
                .await
                .expect("insert");
            backdate_status_change(&pool, id, 7200.0).await;
            spared.push((id, status));
        }

        let deleted = store
            .sweep_dropped(Duration::from_secs(3600))
            .await
            .expect("sweep");

        assert_eq!(deleted, 0);
        for (id, status) in spared {
            assert!(
                store.get(id).await.expect("get").is_some(),
                "{status} proposal {id} must survive the sweep"
            );
        }
    }

    /// A swept proposal takes its auction-participation rows with it;
    /// settled proposals are never swept, so their rows survive — that is
    /// how "solutions rows tied to settlements are never swept" holds
    /// (COW-1204).
    #[ignore]
    #[tokio::test]
    async fn sweep_cascades_solutions_rows_of_dropped_proposals_only() {
        let (store, _audit, pool) = test_store_with_pool().await;
        let dropped = store
            .insert(test_proposal(
                test_order_uid(),
                SOLVER_A,
                ProposalStatus::Cancelled,
            ))
            .await
            .expect("insert");
        let settled = store
            .insert(test_proposal(
                OrderUid([0xbb; 56]),
                SOLVER_A,
                ProposalStatus::Settled,
            ))
            .await
            .expect("insert");
        store
            .record_solution(1, 1, dropped)
            .await
            .expect("record dropped bid");
        store
            .record_solution(2, 1, settled)
            .await
            .expect("record settled bid");
        for id in [dropped, settled] {
            backdate_status_change(&pool, id, 7200.0).await;
        }

        let deleted = store
            .sweep_dropped(Duration::from_secs(3600))
            .await
            .expect("sweep");
        assert_eq!(deleted, 1, "only the cancelled proposal is swept");

        let remaining: Vec<(i64,)> =
            sqlx::query_as("SELECT proposal_id FROM solutions ORDER BY proposal_id")
                .fetch_all(&pool)
                .await
                .expect("solutions query");
        assert_eq!(
            remaining,
            vec![(as_db_id(settled).expect("db-assigned id"),)],
            "the dropped proposal's participation row cascades away; the settlement's survives"
        );
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

        // The variant matters, not just the failure: `is_err()` would also
        // accept a connection blip or an unreadable row, neither of which
        // proves the compare-and-swap did its job.
        assert!(
            matches!(
                stale,
                Err(StoreError::StaleTransition {
                    actual: ProposalStatus::Cancelled,
                    ..
                })
            ),
            "expected the verdict to lose the CAS against Cancelled, got {stale:?}"
        );
        assert_eq!(
            store.get(id).await.expect("get").expect("exists").status,
            ProposalStatus::Cancelled,
            "a stale Accept verdict must not resurrect a cancelled proposal"
        );
    }
}
