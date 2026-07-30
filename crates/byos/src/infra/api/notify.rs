//! `/notify` — the driver's solver-engine notification endpoint (ADR-0010).
//! Settlement outcomes arrive here and drive the `Executing` half of the
//! proposal lifecycle (ADR-0013); attribution goes through the `solutions`
//! table `/solve` writes.

use {
    super::AppState,
    crate::{domain::proposal::SettlementOutcome, infra::storage::OutcomeEffect},
    axum::{Json, extract::State, http::StatusCode},
};

/// Wire notification, mirroring `cowprotocol/services`'s solvers-dto shape.
/// Deliberately our own type rather than the vendored `solvers_dto`: the
/// pinned rev predates `settlementStarted`, and a tagged enum would reject
/// unknown kinds at deserialization — this handler must tolerate anything
/// the driver sends (ADR-0010).
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    /// Decimal string on the wire; optional — some kinds fire before an
    /// auction id exists (e.g. `deserializationError`).
    #[serde(default)]
    auction_id: Option<String>,
    #[serde(default)]
    solution_id: Option<SolutionIds>,
    /// camelCase kind tag. Kept as a string so unknown kinds deserialize.
    kind: String,
    /// Settlement tx hash, present on `success` and `revert`.
    #[serde(default)]
    transaction: Option<alloy::primitives::B256>,
}

/// One solution id, or several when the driver merged solutions into one
/// settlement.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum SolutionIds {
    Single(u64),
    Merged(Vec<u64>),
}

/// POST /notify — internal listener, same trust boundary as `/solve`.
///
/// Always answers 200: the driver fires and forgets, and a notification we
/// cannot act on (unattributable, stale, unknown kind) is something to log,
/// not an error to hand back.
pub async fn notify(
    State(state): State<AppState>,
    Json(notification): Json<Notification>,
) -> StatusCode {
    tracing::debug!(?notification, "driver notification received");

    let proposals = attributed_proposals(&state, &notification).await;

    if proposals.is_empty() {
        if is_outcome(&notification.kind) {
            // An outcome we cannot join to a solution means the solutions
            // record is broken or lost — alert-worthy (ADR-0010).
            tracing::error!(
                kind = %notification.kind,
                auction_id = ?notification.auction_id,
                "unattributable outcome notification"
            );
        } else {
            tracing::debug!(kind = %notification.kind, "unattributable notification ignored");
        }
        return StatusCode::OK;
    }

    let Some(outcome) = outcome_of(&notification) else {
        // Kinds that carry no transition: pre-submission rejections, future
        // additions, and outcome kinds whose tx hash is missing (see
        // `outcome_of`). Evidence only, no row mutation (ADR-0013).
        for proposal in &proposals {
            if is_outcome(&notification.kind) {
                tracing::error!(
                    id = %proposal.id, kind = %notification.kind,
                    "outcome notification without a tx hash; left for the executing timeout"
                );
            }
            state
                .store()
                .note_driver_notification(proposal, &notification.kind);
        }
        return StatusCode::OK;
    };

    for proposal in &proposals {
        // The status is decided inside the store under `FOR UPDATE`; nothing
        // here branches on `proposal.status`, which is already stale by the
        // time this runs.
        match state
            .store()
            .apply_settlement_outcome(proposal, outcome)
            .await
        {
            Ok(OutcomeEffect::Applied { from, to }) => tracing::info!(
                id = %proposal.id, kind = %notification.kind, %from, %to,
                "settlement outcome recorded"
            ),
            // Not legal from the committed status: a duplicate notification,
            // or a cancellation got there first.
            //
            // Recorded as evidence either way, because after the retention
            // sweep the log line is all that would remain of "the driver told
            // us something we did not act on" — and for `Reverted` that is a
            // charge nobody collected.
            Ok(OutcomeEffect::Ignored { from }) => {
                state
                    .store()
                    .note_driver_notification(proposal, &notification.kind);
                if outcome.is_chargeable() {
                    tracing::error!(
                        id = %proposal.id, kind = %notification.kind, %from,
                        "chargeable outcome ignored; the sub-solver may go uncharged"
                    );
                } else {
                    tracing::warn!(
                        id = %proposal.id, kind = %notification.kind, %from,
                        "settlement outcome not applicable to the proposal's current status"
                    );
                }
            }
            // A write we could not perform on a money path: the debit that
            // should follow a revert now depends on the timeout backstop.
            Err(e) => tracing::error!(
                id = %proposal.id, kind = %notification.kind, %e,
                "settlement outcome not recorded"
            ),
        }
    }

    StatusCode::OK
}

/// Map the wire kind to the outcome it reports, or `None` for the kinds that
/// are evidence only.
///
/// The kind stays a string on the DTO so an unknown one still deserializes
/// (ADR-0010), but the vocabulary is spelled out exactly once here rather than
/// in a dispatch match and a separate `is_outcome` list that could drift.
fn outcome_of(notification: &Notification) -> Option<SettlementOutcome> {
    match notification.kind.as_str() {
        "settlementStarted" => Some(SettlementOutcome::Started),
        // A landed-or-reverted report with no tx hash names no settlement, so
        // there is nothing to record or to charge against. Left `Executing`
        // for the timeout backstop rather than guessed at.
        "success" => Some(SettlementOutcome::Succeeded(transaction(notification)?)),
        "revert" => Some(SettlementOutcome::Reverted(transaction(notification)?)),
        "cancelled" | "expired" | "fail" => Some(SettlementOutcome::Abandoned),
        kind => {
            tracing::debug!(kind, "non-outcome driver notification");
            None
        }
    }
}

/// The tx hash a `success`/`revert` must carry. `None` makes `outcome_of`
/// yield no outcome, so the caller records the notification as evidence and
/// logs it with the proposal id it belongs to.
fn transaction(notification: &Notification) -> Option<alloy::primitives::B256> {
    notification.transaction
}

/// Whether this kind describes the fate of a submitted settlement — the ones
/// whose loss of attribution is alert-worthy (ADR-0010).
fn is_outcome(kind: &str) -> bool {
    matches!(
        kind,
        "settlementStarted" | "success" | "revert" | "cancelled" | "expired" | "fail"
    )
}

/// Joins the notification's `(auction_id, solution_id)` to proposals via the
/// `solutions` table. Empty when the ids are missing, unparsable, or match
/// nothing.
async fn attributed_proposals(
    state: &AppState,
    notification: &Notification,
) -> Vec<crate::domain::proposal::Proposal> {
    let auction_id = notification
        .auction_id
        .as_deref()
        .and_then(|id| id.parse::<i64>().ok());
    let solution_ids = notification.solution_id.as_ref().map(|ids| match ids {
        SolutionIds::Single(id) => vec![*id],
        SolutionIds::Merged(ids) => ids.clone(),
    });
    let (Some(auction_id), Some(solution_ids)) = (auction_id, solution_ids) else {
        return vec![];
    };
    match state
        .store()
        .solution_proposals(auction_id, &solution_ids)
        .await
    {
        Ok(proposals) => proposals,
        Err(e) => {
            tracing::error!(%e, auction_id, "notify: solutions lookup failed");
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::super::{internal_router, tests::test_state},
        crate::domain::proposal::{OrderUid, ProposalId, ProposalStatus, test_proposal},
        alloy::primitives::Address,
        axum::{
            Router,
            body::Body,
            http::{Request, StatusCode},
        },
        tower::ServiceExt,
    };

    const AUCTION_ID: i64 = 77;

    /// Inserts a proposal in `status` and records it as solution 1 of
    /// auction 77 — the state `/notify` finds after a won `/solve` round.
    async fn bid_proposal(state: &super::super::AppState, status: ProposalStatus) -> ProposalId {
        let id = state
            .store()
            .insert(test_proposal(
                OrderUid([0xaa; 56]),
                Address::repeat_byte(0x01),
                status,
            ))
            .await
            .expect("insert");
        state
            .store()
            .record_solution(AUCTION_ID, 1, id)
            .await
            .expect("record solution");
        id
    }

    async fn post_notify(app: &Router, body: &serde_json::Value) -> StatusCode {
        post_notify_with_auth(app, body, None).await
    }

    async fn post_notify_with_auth(
        app: &Router,
        body: &serde_json::Value,
        authorization: Option<&str>,
    ) -> StatusCode {
        let mut request = Request::builder()
            .method("POST")
            .uri("/notify")
            .header("content-type", "application/json");
        if let Some(auth) = authorization {
            request = request.header("authorization", auth);
        }
        app.clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
            .status()
    }

    fn notification(kind: &str) -> serde_json::Value {
        serde_json::json!({
            "auctionId": AUCTION_ID.to_string(),
            "solutionId": 1,
            "kind": kind,
        })
    }

    /// `/notify` mutates proposal state, so it must sit inside the same
    /// bearer guard as `/solve` — a refactor that moved the route outside
    /// the guarded sub-router must fail here.
    #[ignore]
    #[tokio::test]
    async fn notify_sits_behind_the_bearer_guard() {
        let state = test_state().await;
        let app = internal_router(state.clone(), Some("driver-secret"));
        let id = bid_proposal(&state, ProposalStatus::Active).await;

        // Without the token: rejected, and no transition happened.
        let status = post_notify(&app, &notification("settlementStarted")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            state
                .store()
                .get(id)
                .await
                .expect("get")
                .expect("exists")
                .status,
            ProposalStatus::Active,
            "an unauthorized notification must not move the proposal"
        );

        // With it: accepted, and the transition lands.
        let status = post_notify_with_auth(
            &app,
            &notification("settlementStarted"),
            Some("Bearer driver-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            state
                .store()
                .get(id)
                .await
                .expect("get")
                .expect("exists")
                .status,
            ProposalStatus::Executing
        );
    }

    /// Acceptance (COW-1204): `settlementStarted` → `success` ends with the
    /// proposal `Settled` and the tx hash readable on the owner's GET.
    #[ignore]
    #[tokio::test]
    async fn success_after_settlement_started_settles_with_the_tx_hash_on_owner_get() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        let owner = alloy::signers::local::PrivateKeySigner::random();
        let id = state
            .store()
            .insert(test_proposal(
                OrderUid([0xaa; 56]),
                owner.address(),
                ProposalStatus::Active,
            ))
            .await
            .expect("insert");
        state
            .store()
            .record_solution(AUCTION_ID, 1, id)
            .await
            .expect("record solution");

        assert_eq!(
            post_notify(&app, &notification("settlementStarted")).await,
            StatusCode::OK
        );
        let tx = format!("0x{}", "11".repeat(32));
        let mut success = notification("success");
        success["transaction"] = serde_json::json!(tx);
        assert_eq!(post_notify(&app, &success).await, StatusCode::OK);

        let header = super::super::tests::read_auth_header(&owner, &state).await;
        let (status, body) =
            super::super::tests::get(state, &format!("/proposal/{id}"), Some(&header)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "settled");
        assert_eq!(body["settlementTxHash"], tx);
    }

    /// Acceptance (COW-1204): `revert` ends `SettleFailed` with the
    /// reverted tx hash recorded on the proposal.
    #[ignore]
    #[tokio::test]
    async fn revert_marks_the_proposal_settle_failed_with_the_tx_hash() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        let id = bid_proposal(&state, ProposalStatus::Executing).await;

        let tx = format!("0x{}", "22".repeat(32));
        let mut revert = notification("revert");
        revert["transaction"] = serde_json::json!(tx);
        assert_eq!(post_notify(&app, &revert).await, StatusCode::OK);

        let proposal = state.store().get(id).await.expect("get").expect("exists");
        assert_eq!(proposal.status, ProposalStatus::SettleFailed);
        assert_eq!(
            proposal.settlement_tx_hash.map(|t| format!("{t:#x}")),
            Some(tx)
        );
    }

    /// Acceptance (COW-1204): `cancelled`/`expired`/`fail` mean no tx landed
    /// — the proposal returns to `Active` and re-enters competition.
    #[ignore]
    #[tokio::test]
    async fn abandoned_submission_returns_the_proposal_to_active() {
        for kind in ["cancelled", "expired", "fail"] {
            let state = test_state().await;
            let app = internal_router(state.clone(), None);
            let id = bid_proposal(&state, ProposalStatus::Executing).await;

            assert_eq!(post_notify(&app, &notification(kind)).await, StatusCode::OK);

            assert_eq!(
                state
                    .store()
                    .get(id)
                    .await
                    .expect("get")
                    .expect("exists")
                    .status,
                ProposalStatus::Active,
                "{kind} must release the proposal back into competition"
            );
        }
    }

    /// Acceptance (COW-1205): a driver-confirmed abandonment ("won but never
    /// settled", ADR-0003) queues the 0.1 × c_l non-settlement debit — the
    /// proposal itself re-enters competition, so the pending charge lives in
    /// the `penalties` queue, not in proposal state.
    #[ignore]
    #[tokio::test]
    async fn abandoned_submission_queues_a_non_settlement_penalty() {
        for kind in ["cancelled", "expired", "fail"] {
            let state = test_state().await;
            let app = internal_router(state.clone(), None);
            let id = bid_proposal(&state, ProposalStatus::Executing).await;

            assert_eq!(post_notify(&app, &notification(kind)).await, StatusCode::OK);

            let pending = state
                .store()
                .pending_penalties()
                .await
                .expect("pending penalties");
            assert_eq!(
                pending.len(),
                1,
                "{kind} must queue exactly one non-settlement penalty"
            );
            assert_eq!(pending[0].proposal_id, id);
            assert_eq!(pending[0].sub_solver, Address::repeat_byte(0x01));
        }
    }

    /// A duplicate abandonment notification finds the proposal already
    /// `Active` — the stale-outcome guard drops it, so the sub-solver is
    /// not charged twice for one lost settlement.
    #[ignore]
    #[tokio::test]
    async fn duplicate_abandonment_does_not_queue_a_second_penalty() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        let _id = bid_proposal(&state, ProposalStatus::Executing).await;

        assert_eq!(
            post_notify(&app, &notification("fail")).await,
            StatusCode::OK
        );
        assert_eq!(
            post_notify(&app, &notification("fail")).await,
            StatusCode::OK
        );

        let pending = state
            .store()
            .pending_penalties()
            .await
            .expect("pending penalties");
        assert_eq!(pending.len(), 1, "one lost settlement, one charge");
    }

    /// The wire format is whatever `cowprotocol/services` sends — including
    /// kinds and payload fields this service never acts on. Every shape
    /// must deserialize; unknown kinds must not 4xx/5xx the driver.
    #[test]
    fn upstream_notification_shapes_deserialize() {
        let bodies = [
            serde_json::json!({"auctionId": "1234", "solutionId": 1, "kind": "settlementStarted"}),
            serde_json::json!({"auctionId": "1234", "solutionId": [1, 2], "kind": "success",
                "transaction": format!("0x{}", "ab".repeat(32))}),
            // Pre-solution kinds fire with no ids at all.
            serde_json::json!({"kind": "deserializationError", "reason": "bad json"}),
            serde_json::json!({"auctionId": null, "solutionId": null, "kind": "timeout"}),
            // Flattened kind-specific fields must be tolerated, not rejected.
            serde_json::json!({"auctionId": "9", "solutionId": 3, "kind": "simulationFailed",
                "block": 123, "succeededOnce": false}),
            // Kinds added upstream after this code shipped.
            serde_json::json!({"auctionId": "9", "solutionId": 3, "kind": "someFutureKind"}),
        ];
        for body in bodies {
            serde_json::from_value::<super::Notification>(body.clone())
                .unwrap_or_else(|e| panic!("must deserialize {body}: {e}"));
        }
    }

    /// Acceptance (COW-1204): a notification with no matching `solutions`
    /// row is acknowledged, not errored — outcome kinds additionally alert
    /// via the log (not observable here).
    #[ignore]
    #[tokio::test]
    async fn unattributable_notifications_are_acknowledged() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        // No solutions row for auction 999, and no ids at all.
        let bodies = [
            serde_json::json!({"auctionId": "999", "solutionId": 5, "kind": "success",
                "transaction": format!("0x{}", "ab".repeat(32))}),
            serde_json::json!({"kind": "timeout"}),
            serde_json::json!({"auctionId": "999", "solutionId": 5, "kind": "someFutureKind"}),
        ];
        for body in bodies {
            assert_eq!(
                post_notify(&app, &body).await,
                StatusCode::OK,
                "driver notifications are fire-and-forget: {body}"
            );
        }
    }

    #[ignore]
    #[tokio::test]
    async fn attributable_non_outcome_kind_changes_nothing() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        let id = bid_proposal(&state, ProposalStatus::Active).await;

        assert_eq!(
            post_notify(&app, &notification("emptySolution")).await,
            StatusCode::OK
        );

        assert_eq!(
            state
                .store()
                .get(id)
                .await
                .expect("get")
                .expect("exists")
                .status,
            ProposalStatus::Active,
            "pre-submission kinds carry no transition (ADR-0013)"
        );
    }

    #[ignore]
    #[tokio::test]
    async fn settlement_started_moves_the_won_proposal_to_executing() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        let id = bid_proposal(&state, ProposalStatus::Active).await;

        let status = post_notify(&app, &notification("settlementStarted")).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            state
                .store()
                .get(id)
                .await
                .expect("get")
                .expect("exists")
                .status,
            ProposalStatus::Executing
        );
    }
}
