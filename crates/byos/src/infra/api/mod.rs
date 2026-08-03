//! HTTP API, served as two listeners with opposite trust boundaries
//! the public one carries the proposal CRUD endpoints for
//! sub-solvers, the internal one carries `/solve` for the co-deployed driver.

pub mod dto;
pub mod error;
pub mod notify;
pub mod routes;
pub mod solve;

use {
    crate::infra::storage::ProposalStore,
    alloy::sol_types::Eip712Domain,
    axum::{
        Router,
        response::IntoResponse,
        routing::{delete, get, post},
    },
    std::{
        net::SocketAddr,
        sync::{Arc, atomic::AtomicU64},
    },
    tokio::sync::oneshot,
};

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

struct AppStateInner {
    store: Arc<ProposalStore>,
    domain: Eip712Domain,
    /// Last-seen `effective_gas_price` from the auction payload (written by
    /// `/solve`, read by the background escrow validator). Seeded with
    /// `--default-gas-price` at startup.
    gas_price: Arc<AtomicU64>,
    /// Lifetime cap (ADR-0013): `POST` rejects `validUntil` further out than
    /// now + this many seconds.
    max_proposal_lifetime_secs: u64,
    /// `HooksTrampoline` contract address for encoding order hooks in
    /// `/solve` solutions. `None` when `--hooks-trampoline` is not set.
    hooks_trampoline: Option<alloy::primitives::Address>,
}

/// Shared application state, cheaply cloneable via `Arc`. The store is
/// separately `Arc`ed because the background validation loop shares it.
#[derive(Clone)]
pub struct AppState(Arc<AppStateInner>);

impl AppState {
    pub fn new(
        store: Arc<ProposalStore>,
        domain: Eip712Domain,
        gas_price: Arc<AtomicU64>,
        max_proposal_lifetime_secs: u64,
        hooks_trampoline: Option<alloy::primitives::Address>,
    ) -> Self {
        Self(Arc::new(AppStateInner {
            store,
            domain,
            gas_price,
            max_proposal_lifetime_secs,
            hooks_trampoline,
        }))
    }

    pub fn store(&self) -> &ProposalStore {
        &self.0.store
    }

    pub fn domain(&self) -> &Eip712Domain {
        &self.0.domain
    }

    pub fn gas_price(&self) -> &Arc<AtomicU64> {
        &self.0.gas_price
    }

    pub fn max_proposal_lifetime_secs(&self) -> u64 {
        self.0.max_proposal_lifetime_secs
    }

    pub fn hooks_trampoline(&self) -> Option<alloy::primitives::Address> {
        self.0.hooks_trampoline
    }
}

// ---------------------------------------------------------------------------
// Router + serve
// ---------------------------------------------------------------------------

/// Internet-facing router: proposal CRUD + health check. `/solve` must never
/// be mounted here — the proposal book it returns (amounts, routes,
/// signatures) is MEV-relevant, so only the co-deployed driver may read it
/// itself.
fn public_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(routes::healthz))
        .route("/proposals", post(routes::create_proposal))
        .route("/proposal/{id}", get(routes::get_proposal))
        .route("/proposal/{id}", delete(routes::cancel_proposal))
        .route("/proposals/{order_uid}", get(routes::list_proposals))
        .route(
            "/proposals/by-sub-solver",
            get(routes::list_proposals_by_sub_solver),
        )
        .with_state(state)
}

/// Driver-facing router: `/solve` + `/notify` + health check, served on an
/// internal bind address that only the co-deployed driver reaches
/// itself. When `solve_bearer_token` is set, both endpoints require
/// `Authorization: Bearer <token>` — the driver sends its configured
/// `[solver.request-headers]` on every request, notifications included.
/// `/healthz` stays open for probes.
fn internal_router(state: AppState, solve_bearer_token: Option<&str>) -> Router {
    let mut solve = Router::new()
        .route("/solve", post(solve::solve))
        .route("/notify", post(notify::notify));
    if let Some(token) = solve_bearer_token {
        let expected = token.to_owned();
        solve = solve.route_layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let expected = expected.clone();
                async move {
                    // RFC 7235: the auth scheme name is case-insensitive
                    // ("Bearer" == "bearer"); the token itself is not.
                    let authorized = req
                        .headers()
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.split_once(' '))
                        .is_some_and(|(scheme, token)| {
                            scheme.eq_ignore_ascii_case("Bearer") && token == expected
                        });
                    if authorized {
                        next.run(req).await
                    } else {
                        (
                            axum::http::StatusCode::UNAUTHORIZED,
                            [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
                        )
                            .into_response()
                    }
                }
            },
        ));
    }
    Router::new()
        .route("/healthz", get(routes::healthz))
        .merge(solve)
        .with_state(state)
}

/// Typed error for the API servers (ADR-0007: library functions avoid
/// `anyhow::Result`; callers can match on failure modes).
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("failed to bind listener")]
    Bind(#[source] std::io::Error),
    #[error("server error")]
    Serve(#[source] std::io::Error),
}

/// Where the two listeners actually bound — tests bind port 0 and need the
/// resolved addresses back.
#[derive(Debug, Clone, Copy)]
pub struct BoundAddrs {
    pub public: SocketAddr,
    pub internal: SocketAddr,
}

/// Bind both listeners, serve, and wait for graceful shutdown (ctrl-c, or
/// `shutdown_rx` so tests can stop an in-process instance).
pub async fn serve(
    public_addr: SocketAddr,
    internal_addr: SocketAddr,
    state: AppState,
    solve_bearer_token: Option<&str>,
    bind_tx: Option<oneshot::Sender<BoundAddrs>>,
    shutdown_rx: Option<oneshot::Receiver<()>>,
) -> Result<(), ServeError> {
    let public = public_router(state.clone());
    let internal = internal_router(state, solve_bearer_token);

    let public_listener = tokio::net::TcpListener::bind(public_addr)
        .await
        .map_err(ServeError::Bind)?;
    let internal_listener = tokio::net::TcpListener::bind(internal_addr)
        .await
        .map_err(ServeError::Bind)?;
    let bound = BoundAddrs {
        public: public_listener.local_addr().map_err(ServeError::Bind)?,
        internal: internal_listener.local_addr().map_err(ServeError::Bind)?,
    };

    tracing::info!(public = %bound.public, internal = %bound.internal, "serving API");

    if let Some(tx) = bind_tx {
        let _ = tx.send(bound);
    }

    // One shutdown signal fanned out to both servers: when the spawned task
    // drops the watch sender, every receiver's `changed()` resolves.
    let (watch_tx, watch_rx) = tokio::sync::watch::channel(());
    // Owned, because this task parks on ctrl_c for the process lifetime. The
    // path that reaches the abort below with it still parked is `try_join!`
    // returning early because one listener errored: without the abort, the
    // task outlives `serve` holding a signal registration and the shutdown
    // oneshot. (A bind failure cannot get here — both `?`s above return before
    // the spawn.) Each task owns its own watch sender, wired only to its own
    // servers, so a leaked one cannot reach a later instance.
    let signal = tokio::spawn(async move {
        shutdown_signal(shutdown_rx).await;
        drop(watch_tx);
    });
    let stop = |mut rx: tokio::sync::watch::Receiver<()>| async move {
        let _ = rx.changed().await;
    };

    let served = tokio::try_join!(
        axum::serve(public_listener, public).with_graceful_shutdown(stop(watch_rx.clone())),
        axum::serve(internal_listener, internal).with_graceful_shutdown(stop(watch_rx)),
    );
    signal.abort();
    served.map_err(ServeError::Serve)?;

    Ok(())
}

async fn shutdown_signal(shutdown_rx: Option<oneshot::Receiver<()>>) {
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    match shutdown_rx {
        Some(rx) => {
            tokio::select! {
                _ = ctrl_c => {}
                _ = rx => {}
            }
        }
        None => {
            ctrl_c.await.ok();
        }
    }
    tracing::info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::domain::proposal::{OrderUid, ProposalStatus, test_proposal},
        alloy::{
            primitives::{Address, U256, address, keccak256},
            signers::local::PrivateKeySigner,
        },
        axum::{
            body::Body,
            http::{Request, StatusCode},
        },
        byos_common::{contracts, eip712},
        std::sync::atomic::{AtomicU64, Ordering},
        tower::ServiceExt,
    };

    const CHAIN_ID: u64 = 1;

    fn factory() -> Address {
        Address::repeat_byte(0x42)
    }

    /// Router tests are `#[ignore]`d db-tier tests (`just test-db`): the
    /// proposal store is Postgres (ADR-0013), so each test gets a fresh
    /// database via the service-test harness. `pub(super)` so sibling route
    /// modules (`notify`) reuse the same harness.
    pub(super) async fn test_state() -> AppState {
        // These router tests assert on HTTP behaviour, not audit evidence.
        // Leaking the receiver keeps the channel open so emits stay silent.
        let (audit_tx, audit_rx) = tokio::sync::mpsc::unbounded_channel();
        std::mem::forget(audit_rx);
        let db = crate::tests::setup::TestDb::create().await;
        let pool = crate::infra::audit::connect_and_migrate(&db.url)
            .await
            .expect("migrations run");
        let domain = eip712::byos_domain(CHAIN_ID, factory());
        let gas_price = Arc::new(AtomicU64::new(0));
        AppState::new(
            Arc::new(ProposalStore::new(pool, audit_tx)),
            domain,
            gas_price,
            300,
            None,
        )
    }

    /// Builds a valid signed POST /proposals JSON body and returns it along
    /// with the signer's address. The signature covers `interactions` and the
    /// body is rendered from that same slice, so a caller cannot accidentally
    /// sign one route and post another.
    async fn signed_proposal_body_for(
        signer: &PrivateKeySigner,
        interactions: &[contracts::Interaction],
    ) -> (serde_json::Value, Address) {
        let domain = eip712::byos_domain(CHAIN_ID, factory());

        // Inside the 5-minute lifetime cap the test_state applies.
        let valid_until = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_secs()
            + 240;
        let order_uid = [0xaa_u8; 56];
        let proposal = contracts::Proposal {
            orderUidHash: keccak256(order_uid),
            sellAmount: U256::from(1_000_000_u64),
            buyAmount: U256::from(990_000_u64),
            validUntil: U256::from(valid_until),
            nonce: U256::from(1_u64),
        };
        let signature = eip712::sign_proposal(signer, &domain, &proposal, interactions)
            .await
            .expect("signing must succeed");

        let body = serde_json::json!({
            "orderUid": format!("0x{}", alloy::hex::encode(order_uid)),
            "sellAmount": "1000000",
            "buyAmount": "990000",
            "interactions": interactions.iter().map(interaction_json).collect::<Vec<_>>(),
            "validUntil": valid_until.to_string(),
            "nonce": "1",
            "signature": format!("0x{}", alloy::hex::encode(signature.as_bytes())),
        });
        (body, signer.address())
    }

    /// Renders one contract interaction as the wire shape `proposal-dto`
    /// expects: decimal string value, hex callData (ADR-0005).
    fn interaction_json(i: &contracts::Interaction) -> serde_json::Value {
        serde_json::json!({
            "target": i.target.to_string(),
            "value": i.value.to_string(),
            "callData": format!("0x{}", alloy::hex::encode(&i.callData)),
        })
    }

    /// A two-hop route: an ERC20 approve-shaped call and a swap-shaped call.
    /// Contents are opaque to the service — it stores and re-encodes them
    /// without interpreting them — so only the shape has to be realistic.
    fn sample_route() -> Vec<contracts::Interaction> {
        vec![
            contracts::Interaction {
                target: address!("3333333333333333333333333333333333333333"),
                value: U256::ZERO,
                callData: alloy::primitives::hex!("095ea7b3deadbeef").into(),
            },
            contracts::Interaction {
                target: address!("4444444444444444444444444444444444444444"),
                value: U256::from(7_u64),
                callData: alloy::primitives::hex!("128acb08cafe").into(),
            },
        ]
    }

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body must be readable");
        serde_json::from_slice(&bytes).expect("body must be JSON")
    }

    async fn post_proposal(app: &Router, body: &serde_json::Value) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/proposals")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    struct RejectAll(crate::domain::validator::RejectionReason);

    impl crate::domain::validator::ValidateProposal for RejectAll {
        async fn validate(
            &self,
            _proposal: &crate::domain::proposal::Proposal,
        ) -> Option<crate::domain::validator::Verdict> {
            Some(crate::domain::validator::Verdict::Reject(self.0))
        }
    }

    /// POSTs a proposal, rejects it with `reason`, and returns the owner's
    /// `GET /proposal/{id}` body.
    async fn rejected_proposal_body(
        reason: crate::domain::validator::RejectionReason,
    ) -> serde_json::Value {
        let state = test_state().await;
        let app = public_router(state.clone());
        let signer = PrivateKeySigner::random();
        let (body, _) = signed_proposal_body_for(&signer, &[]).await;

        let response = post_proposal(&app, &body).await;
        let id = json_body(response).await["id"].as_u64().expect("id");

        crate::infra::validation::run_tick(
            state.store(),
            &RejectAll(reason),
            0,
            std::time::Duration::from_secs(3600),
        )
        .await;

        let header = read_auth_header(&signer, &state).await;
        let (status, body) = get(state, &format!("/proposal/{id}"), Some(&header)).await;
        assert_eq!(status, StatusCode::OK);
        body
    }

    #[ignore]
    #[tokio::test]
    async fn rejected_proposal_exposes_reason_on_the_wire() {
        let body =
            rejected_proposal_body(crate::domain::validator::RejectionReason::InsufficientEscrow)
                .await;
        assert_eq!(body["status"], "rejected");
        assert_eq!(body["rejectionReason"], "InsufficientEscrow");
    }

    #[ignore]
    #[tokio::test]
    async fn unprofitable_rejection_exposes_reason_on_the_wire() {
        let body =
            rejected_proposal_body(crate::domain::validator::RejectionReason::Unprofitable).await;
        assert_eq!(body["status"], "rejected");
        assert_eq!(body["rejectionReason"], "Unprofitable");
    }

    #[ignore]
    #[tokio::test]
    async fn double_cancel_returns_conflict() {
        use alloy::sol_types::SolStruct;

        let domain = eip712::byos_domain(CHAIN_ID, factory());
        let state = test_state().await;
        let app = public_router(state);

        let signer = PrivateKeySigner::random();
        let (body, _) = signed_proposal_body_for(&signer, &[]).await;
        let response = post_proposal(&app, &body).await;
        let id = json_body(response).await["id"].as_u64().expect("id");

        let cancel = eip712::CancelProposal {
            proposalId: U256::from(id),
        };
        let signature =
            alloy::signers::Signer::sign_hash(&signer, &cancel.eip712_signing_hash(&domain))
                .await
                .expect("signing must succeed");

        let delete = |app: Router| {
            let sig_hex = format!("0x{}", alloy::hex::encode(signature.as_bytes()));
            async move {
                app.oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri(format!("/proposal/{id}"))
                        .header("X-Signature", sig_hex)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };

        // Cancelling a Submitted proposal works…
        let first = delete(app.clone()).await;
        assert_eq!(first.status(), StatusCode::NO_CONTENT);

        // …cancelling it again conflicts with its terminal state.
        let second = delete(app.clone()).await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    /// Acceptance: a settlement is in flight — the owner cannot
    /// pull the proposal out from under it. `DELETE` conflicts.
    #[ignore]
    #[tokio::test]
    async fn cancel_of_an_executing_proposal_returns_conflict() {
        use alloy::sol_types::SolStruct;

        let domain = eip712::byos_domain(CHAIN_ID, factory());
        let state = test_state().await;
        let owner = PrivateKeySigner::random();
        let id = state
            .store()
            .insert(test_proposal(
                OrderUid([0xaa; 56]),
                owner.address(),
                ProposalStatus::Executing,
            ))
            .await
            .expect("insert");

        let cancel = eip712::CancelProposal {
            proposalId: U256::from(id.0),
        };
        let signature =
            alloy::signers::Signer::sign_hash(&owner, &cancel.eip712_signing_hash(&domain))
                .await
                .expect("signing must succeed");

        let response = public_router(state.clone())
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/proposal/{id}"))
                    .header(
                        "X-Signature",
                        format!("0x{}", alloy::hex::encode(signature.as_bytes())),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            state
                .store()
                .get(id)
                .await
                .expect("get")
                .expect("exists")
                .status,
            ProposalStatus::Executing,
            "the in-flight settlement keeps its proposal"
        );
    }

    #[ignore]
    #[tokio::test]
    async fn cancel_by_non_owner_is_masked_as_not_found() {
        use alloy::sol_types::SolStruct;

        let domain = eip712::byos_domain(CHAIN_ID, factory());
        let state = test_state().await;
        let owner = address!("0000000000000000000000000000000000000001");
        let id = insert_proposal(&state, owner).await;

        let intruder = PrivateKeySigner::random();
        let cancel = eip712::CancelProposal {
            proposalId: U256::from(id.0),
        };
        let signature =
            alloy::signers::Signer::sign_hash(&intruder, &cancel.eip712_signing_hash(&domain))
                .await
                .expect("signing must succeed");

        let response = public_router(state.clone())
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/proposal/{id}"))
                    .header(
                        "X-Signature",
                        format!("0x{}", alloy::hex::encode(signature.as_bytes())),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Same 404 as a genuine miss — a 403 would be an existence oracle
        // for proposal ids (ADR-0011).
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(json_body(response).await["kind"], "ProposalNotFound");

        // The proposal is untouched.
        let proposal = state
            .store()
            .get(id)
            .await
            .expect("get succeeds")
            .expect("proposal must still exist");
        assert_eq!(proposal.status, ProposalStatus::Active);
    }

    #[ignore]
    #[tokio::test]
    async fn post_without_token_fields_is_accepted() {
        // Token addresses come from the orderbook (ADR-0012), not the
        // sub-solver; the API contract must not require them.
        let state = test_state().await;
        let app = public_router(state);
        let signer = PrivateKeySigner::random();
        let (mut body, _) = signed_proposal_body_for(&signer, &[]).await;
        body.as_object_mut().unwrap().remove("sellToken");
        body.as_object_mut().unwrap().remove("buyToken");

        let response = post_proposal(&app, &body).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    #[ignore]
    #[tokio::test]
    async fn post_returns_202_and_proposal_is_submitted() {
        let state = test_state().await;
        let app = public_router(state.clone());
        let signer = PrivateKeySigner::random();
        let (body, _) = signed_proposal_body_for(&signer, &[]).await;

        let response = post_proposal(&app, &body).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let id = json_body(response).await["id"].as_u64().expect("id");

        let header = read_auth_header(&signer, &state).await;
        let (status, json) = get(state, &format!("/proposal/{id}"), Some(&header)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["status"], "submitted");
    }

    async fn insert_proposal(
        state: &AppState,
        sub_solver: Address,
    ) -> crate::domain::proposal::ProposalId {
        state
            .store()
            .insert(test_proposal(
                OrderUid([0xaa; 56]),
                sub_solver,
                ProposalStatus::Active,
            ))
            .await
            .expect("insert succeeds")
    }

    /// Signs the `ReadAuth` bearer message and formats it for `X-Signature`.
    pub(super) async fn read_auth_header(signer: &PrivateKeySigner, state: &AppState) -> String {
        let sig = byos_common::eip712::sign_read_auth(signer, state.domain())
            .await
            .expect("signing should succeed");
        format!("0x{}", alloy::hex::encode(sig.as_bytes()))
    }

    /// Fires a GET at the router, optionally with an `X-Signature` header.
    /// Returns the status and parsed JSON body.
    pub(super) async fn get(
        state: AppState,
        uri: &str,
        signature: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder().uri(uri);
        if let Some(sig) = signature {
            request = request.header("X-Signature", sig);
        }
        let response = public_router(state)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[ignore]
    #[tokio::test]
    async fn get_proposal_owner_reads_own() {
        let state = test_state().await;
        let owner = alloy::signers::local::PrivateKeySigner::random();
        let id = insert_proposal(&state, owner.address()).await;
        let header = read_auth_header(&owner, &state).await;

        let (status, json) = get(state, &format!("/proposal/{id}"), Some(&header)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["id"], id.0);
        assert_eq!(json["sellAmount"], "1000000");
        assert_eq!(json["buyAmount"], "990000");
    }

    /// Acceptance: once the Track A debit lands, the owner's GET
    /// shows `penalized` and cites the debit tx.
    #[ignore]
    #[tokio::test]
    async fn penalized_proposal_exposes_the_penalty_tx_on_owner_get() {
        let state = test_state().await;
        let owner = alloy::signers::local::PrivateKeySigner::random();
        let mut proposal = test_proposal(
            OrderUid([0xaa; 56]),
            owner.address(),
            ProposalStatus::SettleFailed,
        );
        let settlement_tx = format!("0x{}", "22".repeat(32));
        proposal.settlement_tx_hash = Some(settlement_tx.parse().unwrap());
        let id = state.store().insert(proposal).await.expect("insert");

        let penalty_tx = format!("0x{}", "77".repeat(32));
        let stored = state.store().get(id).await.expect("get").expect("exists");
        state
            .store()
            .record_penalty(
                &stored,
                alloy::primitives::U256::from(16_000_000_000_000_000u64),
                penalty_tx.parse().unwrap(),
            )
            .await
            .expect("debit landed");

        let header = read_auth_header(&owner, &state).await;
        let (status, json) = get(state, &format!("/proposal/{id}"), Some(&header)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["status"], "penalized");
        assert_eq!(json["penaltyTxHash"], penalty_tx);
        assert_eq!(
            json["settlementTxHash"], settlement_tx,
            "the reverted settlement stays cited alongside the debit"
        );
    }

    #[ignore]
    #[tokio::test]
    async fn get_proposal_non_owner_gets_404() {
        let state = test_state().await;
        let owner = address!("0000000000000000000000000000000000000001");
        let id = insert_proposal(&state, owner).await;

        let other = alloy::signers::local::PrivateKeySigner::random();
        let header = read_auth_header(&other, &state).await;

        let (status, _) = get(state, &format!("/proposal/{id}"), Some(&header)).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[ignore]
    #[tokio::test]
    async fn list_by_order_uid_scoped_to_caller() {
        let state = test_state().await;
        let caller = alloy::signers::local::PrivateKeySigner::random();
        let competitor = address!("0000000000000000000000000000000000000002");

        // Two proposals on the same order UID, different sub-solvers.
        insert_proposal(&state, caller.address()).await;
        insert_proposal(&state, competitor).await;

        let header = read_auth_header(&caller, &state).await;
        let uid_hex = format!("0x{}", alloy::hex::encode([0xaa; 56]));

        let (status, json) = get(state, &format!("/proposals/{uid_hex}"), Some(&header)).await;

        assert_eq!(status, StatusCode::OK);
        let proposals = json["proposals"].as_array().unwrap();
        assert_eq!(proposals.len(), 1, "competitor's proposal must not leak");
        let returned: Address = proposals[0]["subSolver"].as_str().unwrap().parse().unwrap();
        assert_eq!(returned, caller.address());
    }

    #[ignore]
    #[tokio::test]
    async fn list_by_sub_solver_uses_signer_identity() {
        let state = test_state().await;
        let caller = alloy::signers::local::PrivateKeySigner::random();
        let competitor = address!("0000000000000000000000000000000000000002");

        insert_proposal(&state, caller.address()).await;
        insert_proposal(&state, competitor).await;

        let header = read_auth_header(&caller, &state).await;

        let (status, json) = get(state, "/proposals/by-sub-solver", Some(&header)).await;

        assert_eq!(status, StatusCode::OK);
        let proposals = json["proposals"].as_array().unwrap();
        assert_eq!(proposals.len(), 1);
        let returned: Address = proposals[0]["subSolver"].as_str().unwrap().parse().unwrap();
        assert_eq!(returned, caller.address());
    }

    #[ignore]
    #[tokio::test]
    async fn get_proposal_without_signature_is_rejected() {
        let state = test_state().await;
        let sub_solver = address!("0000000000000000000000000000000000000001");
        let id = insert_proposal(&state, sub_solver).await;

        let (status, _) = get(state, &format!("/proposal/{id}"), None).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    // -----------------------------------------------------------------------
    // /solve tests
    // -----------------------------------------------------------------------

    const SELL_TOKEN: Address = address!("1111111111111111111111111111111111111111");
    const BUY_TOKEN: Address = address!("2222222222222222222222222222222222222222");
    const ORDER_UID: [u8; 56] = [0xaa; 56];
    /// Distinct from both tokens on purpose: it is what catches a
    /// `build_solution` that wires the wrong address into the transfer.
    const TRAMPOLINE: Address = address!("5555555555555555555555555555555555555555");

    /// Builds a minimal valid auction JSON with one order.
    /// `gas_price` is the auction's `effectiveGasPrice` as a decimal string,
    /// not a `u64`: `/solve` has to cope with a value that does not fit one,
    /// and only the wire type can express that.
    fn auction_json(
        kind: &str,
        sell_amount: &str,
        buy_amount: &str,
        gas_price: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "tokens": {
                SELL_TOKEN.to_string(): {
                    "referencePrice": "1000000000000000000",
                    "availableBalance": "0",
                    "trusted": false
                },
                BUY_TOKEN.to_string(): {
                    "referencePrice": "1000000000000000000",
                    "availableBalance": "0",
                    "trusted": false
                }
            },
            "orders": [{
                "uid": format!("0x{}", alloy::hex::encode(ORDER_UID)),
                "sellToken": SELL_TOKEN.to_string(),
                "buyToken": BUY_TOKEN.to_string(),
                "sellAmount": sell_amount,
                "fullSellAmount": sell_amount,
                "buyAmount": buy_amount,
                "fullBuyAmount": buy_amount,
                "validTo": 4_294_967_295u32,
                "kind": kind,
                "owner": Address::ZERO.to_string(),
                "partiallyFillable": false,
                "preInteractions": [],
                "postInteractions": [],
                "sellTokenSource": "erc20",
                "buyTokenDestination": "erc20",
                "class": "limit",
                "appData": format!("0x{}", alloy::hex::encode([0u8; 32])),
                "signingScheme": "eip712",
                "signature": "0x"
            }],
            "liquidity": [],
            "effectiveGasPrice": gas_price,
            "deadline": "2099-01-01T00:00:00Z",
            "surplusCapturingJitOrderOwners": []
        })
    }

    /// A sibling of [`insert_active_proposal`] rather than two more parameters
    /// on it: the route and the trampoline are irrelevant to the ten tests that
    /// call that one, and threading them through would only add noise there.
    /// Returns the proposal as stored, so callers assert against real values
    /// instead of restating the fixture.
    async fn insert_routed_proposal(
        state: &AppState,
        sub_solver: Address,
        route: &[contracts::Interaction],
    ) -> crate::domain::proposal::Proposal {
        let mut proposal = test_proposal(OrderUid(ORDER_UID), sub_solver, ProposalStatus::Active);
        proposal.sell_amount = U256::from(1_000_u64);
        proposal.buy_amount = U256::from(950_u64);
        proposal.gas_used = Some(200_000);
        proposal.trampoline = Some(TRAMPOLINE);
        proposal.interactions = route.to_vec();
        proposal.interactions_hash = eip712::compute_interactions_hash(route);
        proposal.signature = alloy::primitives::Bytes::from(vec![0x11_u8; 65]);
        let id = state
            .store()
            .insert(proposal.clone())
            .await
            .expect("insert succeeds");
        proposal.id = id;
        proposal
    }

    async fn insert_active_proposal(
        state: &AppState,
        sub_solver: Address,
        sell_amount: u64,
        buy_amount: u64,
    ) {
        insert_active_proposal_with_gas(state, sub_solver, sell_amount, buy_amount, 200_000).await;
    }

    async fn insert_active_proposal_with_gas(
        state: &AppState,
        sub_solver: Address,
        sell_amount: u64,
        buy_amount: u64,
        gas_used: u64,
    ) {
        let mut proposal = test_proposal(OrderUid(ORDER_UID), sub_solver, ProposalStatus::Active);
        proposal.sell_amount = U256::from(sell_amount);
        proposal.buy_amount = U256::from(buy_amount);
        proposal.gas_used = Some(gas_used);
        proposal.trampoline = Some(Address::ZERO);
        state
            .store()
            .insert(proposal)
            .await
            .expect("insert succeeds");
    }

    async fn raw_post_solve(app: &Router, auction: &serde_json::Value) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/solve")
                    .header("content-type", "application/json")
                    .body(Body::from(auction.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn post_solve(app: &Router, auction: &serde_json::Value) -> serde_json::Value {
        let response = raw_post_solve(app, auction).await;
        assert_eq!(response.status(), StatusCode::OK);
        json_body(response).await
    }

    #[ignore]
    #[tokio::test]
    async fn solve_is_not_reachable_on_the_public_router() {
        let state = test_state().await;
        insert_active_proposal(&state, Address::ZERO, 1_000, 950).await;

        let auction = auction_json("sell", "1000", "900", "0");
        let response = raw_post_solve(&public_router(state), &auction).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[ignore]
    #[tokio::test]
    async fn proposal_endpoints_are_not_reachable_on_the_internal_router() {
        let state = test_state().await;
        let signer = PrivateKeySigner::random();
        let (body, _) = signed_proposal_body_for(&signer, &[]).await;

        let app = internal_router(state, None);
        let response = post_proposal(&app, &body).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/proposals/by-sub-solver")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    async fn post_solve_with_auth(
        app: &Router,
        auction: &serde_json::Value,
        authorization: &str,
    ) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/solve")
                    .header("content-type", "application/json")
                    .header("authorization", authorization)
                    .body(Body::from(auction.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[ignore]
    #[tokio::test]
    async fn solve_without_bearer_token_is_rejected_when_one_is_configured() {
        let state = test_state().await;
        insert_active_proposal(&state, Address::ZERO, 1_000, 950).await;
        let app = internal_router(state, Some("driver-secret"));

        let auction = auction_json("sell", "1000", "900", "0");
        let response = raw_post_solve(&app, &auction).await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        // RFC 7235: a 401 names the expected auth scheme.
        assert_eq!(
            response.headers().get("www-authenticate").unwrap(),
            "Bearer"
        );
    }

    #[ignore]
    #[tokio::test]
    async fn solve_with_the_configured_bearer_token_succeeds() {
        let state = test_state().await;
        insert_active_proposal(&state, Address::ZERO, 1_000, 950).await;
        let app = internal_router(state, Some("driver-secret"));

        let auction = auction_json("sell", "1000", "900", "0");
        let response = post_solve_with_auth(&app, &auction, "Bearer driver-secret").await;

        assert_eq!(response.status(), StatusCode::OK);
        let solutions = json_body(response).await["solutions"].clone();
        assert_eq!(solutions.as_array().unwrap().len(), 1);
    }

    #[ignore]
    #[tokio::test]
    async fn solve_bearer_scheme_is_case_insensitive() {
        let state = test_state().await;
        insert_active_proposal(&state, Address::ZERO, 1_000, 950).await;
        let app = internal_router(state, Some("driver-secret"));

        let auction = auction_json("sell", "1000", "900", "0");
        let response = post_solve_with_auth(&app, &auction, "bearer driver-secret").await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[ignore]
    #[tokio::test]
    async fn solve_with_a_wrong_bearer_token_is_rejected() {
        let state = test_state().await;
        insert_active_proposal(&state, Address::ZERO, 1_000, 950).await;
        let app = internal_router(state, Some("driver-secret"));

        let auction = auction_json("sell", "1000", "900", "0");
        let response = post_solve_with_auth(&app, &auction, "Bearer not-the-secret").await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers().get("www-authenticate").unwrap(),
            "Bearer"
        );
    }

    #[ignore]
    #[tokio::test]
    async fn solve_with_a_case_changed_token_is_rejected() {
        let state = test_state().await;
        insert_active_proposal(&state, Address::ZERO, 1_000, 950).await;
        let app = internal_router(state, Some("driver-secret"));

        let auction = auction_json("sell", "1000", "900", "0");
        let response = post_solve_with_auth(&app, &auction, "Bearer DRIVER-SECRET").await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[ignore]
    #[tokio::test]
    async fn healthz_stays_open_when_a_bearer_token_is_configured() {
        let response = internal_router(test_state().await, Some("driver-secret"))
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[ignore]
    #[tokio::test]
    async fn healthz_responds_on_both_routers() {
        let healthz = |app: Router| async move {
            app.oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        };

        assert_eq!(
            healthz(public_router(test_state().await)).await,
            StatusCode::OK
        );
        assert_eq!(
            healthz(internal_router(test_state().await, None)).await,
            StatusCode::OK
        );
    }

    #[ignore]
    #[tokio::test]
    async fn solve_sell_order_prices_are_cross_multiplied() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        insert_active_proposal(&state, Address::ZERO, 1_000, 950).await;

        let auction = auction_json("sell", "1000", "900", "0");
        let result = post_solve(&app, &auction).await;

        let solutions = result["solutions"].as_array().unwrap();
        assert_eq!(solutions.len(), 1);

        let prices = &solutions[0]["prices"];
        // sell_token price = proposal.buy_amount, buy_token price =
        // proposal.sell_amount
        assert_eq!(prices[SELL_TOKEN.to_string()], "950");
        assert_eq!(prices[BUY_TOKEN.to_string()], "1000");
    }

    #[ignore]
    #[tokio::test]
    async fn solve_sell_order_executed_amount_is_sell() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        insert_active_proposal(&state, Address::ZERO, 1_000, 950).await;

        let auction = auction_json("sell", "1000", "900", "0");
        let result = post_solve(&app, &auction).await;

        let trade = &result["solutions"][0]["trades"][0];
        assert_eq!(trade["executedAmount"], "1000");
    }

    #[ignore]
    #[tokio::test]
    async fn solve_buy_order_executed_amount_is_buy() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        insert_active_proposal(&state, Address::ZERO, 950, 900).await;

        let auction = auction_json("buy", "1000", "900", "0");
        let result = post_solve(&app, &auction).await;

        let trade = &result["solutions"][0]["trades"][0];
        assert_eq!(trade["executedAmount"], "900");
    }

    /// Both tokens priced at parity and 1 wei per gas, so the cut in sell-token
    /// atoms is just the effective gas: 200_000 simulated plus the buffer.
    const CUT_AT_PARITY: u64 = 230_000;

    /// The driver checks `executed + fee == order.sellAmount` on a sell order,
    /// and rejects a solution that declares no fee at all.
    #[ignore]
    #[tokio::test]
    async fn solve_declares_the_gas_cut_on_a_sell_order() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        insert_active_proposal(&state, Address::ZERO, 1_000_000_000, 1_000_000_000).await;

        let auction = auction_json("sell", "1000000000", "900000000", "1");
        let result = post_solve(&app, &auction).await;

        let trade = &result["solutions"][0]["trades"][0];
        assert_eq!(trade["fee"], CUT_AT_PARITY.to_string());
        assert_eq!(
            trade["executedAmount"],
            (1_000_000_000 - CUT_AT_PARITY).to_string()
        );

        // The invariant in the driver's own words, read back off the wire.
        let fee: u64 = trade["fee"].as_str().unwrap().parse().unwrap();
        let executed: u64 = trade["executedAmount"].as_str().unwrap().parse().unwrap();
        assert_eq!(
            executed + fee,
            1_000_000_000,
            "executed + fee must equal the order's sell amount",
        );
    }

    /// A buy order's cut rides alongside an unchanged execution: it sits
    /// outside the driver's check, so the user is protected on the paying
    /// side instead.
    #[ignore]
    #[tokio::test]
    async fn solve_declares_the_gas_cut_on_a_buy_order() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        insert_active_proposal(&state, Address::ZERO, 950_000_000, 900_000_000).await;

        let auction = auction_json("buy", "1000000000", "900000000", "1");
        let result = post_solve(&app, &auction).await;

        let trade = &result["solutions"][0]["trades"][0];
        assert_eq!(trade["executedAmount"], "900000000");
        assert_eq!(trade["fee"], CUT_AT_PARITY.to_string());
    }

    /// If the cut ever moved the price vector, `encode_settle` would stop
    /// producing the transaction ADR-0012 simulated.
    #[ignore]
    #[tokio::test]
    async fn solve_clearing_prices_are_unchanged_by_the_cut() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        insert_active_proposal(&state, Address::ZERO, 1_000_000_000, 1_000_000_000).await;

        let auction = auction_json("sell", "1000000000", "900000000", "1");
        let result = post_solve(&app, &auction).await;

        let prices = &result["solutions"][0]["prices"];
        assert_eq!(prices[SELL_TOKEN.to_string()], "1000000000");
        assert_eq!(prices[BUY_TOKEN.to_string()], "1000000000");
    }

    /// The score alone will not catch a breach. Here the auction prices both
    /// tokens at parity while the route trades them 1:2, so the score sees
    /// 300_000 wei of surplus against a 230_000 wei gas bill and says yes,
    /// while the cut costs the user 460_000 buy-token atoms and breaks
    /// their limit.
    ///
    /// The fat-surplus half proves the fixture can bid at all, so the thin half
    /// is failing on the limit and not on some unrelated filter.
    #[ignore]
    #[tokio::test]
    async fn solve_skips_a_proposal_whose_cut_breaches_the_signed_limit() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        insert_active_proposal(&state, Address::ZERO, 1_000_000_000, 2_000_000_000).await;

        let thin = auction_json("sell", "1000000000", "1999700000", "1");
        let result = post_solve(&app, &thin).await;
        assert!(
            result["solutions"].as_array().unwrap().is_empty(),
            "a cut that breaches the signed buy amount must not be bid",
        );

        let fat = auction_json("sell", "1000000000", "1990000000", "1");
        let result = post_solve(&app, &fat).await;
        assert_eq!(
            result["solutions"].as_array().unwrap().len(),
            1,
            "the same proposal against a limit with room for the cut must bid",
        );
    }

    /// The cut is checked per proposal, before the winner is picked, so a
    /// breach costs us that proposal and not the whole order. Moving the check
    /// after the ranking would pass every other test and silently start
    /// skipping orders that had a perfectly good second choice.
    ///
    /// The top scorer can only be the one that breaches when the two differ in
    /// gas: at equal gas, more `buy_amount` means both a higher score and more
    /// room for the cut. So the fat route here rides an expensive path.
    #[ignore]
    #[tokio::test]
    async fn solve_falls_back_to_the_runner_up_when_the_best_cut_breaches() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        // Cut 530_000, score 470_000, delivers 1_998_940_000 — under the limit.
        insert_active_proposal_with_gas(
            &state,
            Address::ZERO,
            1_000_000_000,
            2_000_000_000,
            500_000,
        )
        .await;
        // Cut 130_000, score 270_000, delivers 1_999_140_078 — over it.
        insert_active_proposal_with_gas(
            &state,
            Address::ZERO,
            1_000_000_000,
            1_999_400_000,
            100_000,
        )
        .await;

        let tight = auction_json("sell", "1000000000", "1999000000", "1");
        let result = post_solve(&app, &tight).await;

        let solutions = result["solutions"].as_array().unwrap();
        assert_eq!(solutions.len(), 1, "the runner-up still deserves a bid");
        assert_eq!(
            solutions[0]["trades"][0]["fee"], "130000",
            "the surviving bid must be the runner-up, not the higher-scoring proposal",
        );
        assert_eq!(solutions[0]["trades"][0]["executedAmount"], "999870000");

        // Same two proposals, a limit with room for both cuts. The fat route
        // wins on score here, which is what makes the run above a fallback and
        // not the ranking picking the cheap proposal on its own merits.
        let loose = auction_json("sell", "1000000000", "1990000000", "1");
        let result = post_solve(&app, &loose).await;

        assert_eq!(result["solutions"][0]["trades"][0]["fee"], "530000");
    }

    /// The driver's settlement budget: simulated gas plus the scoring buffer,
    /// the same number the cut is priced from.
    #[ignore]
    #[tokio::test]
    async fn solve_reports_the_effective_gas_on_the_solution() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        insert_active_proposal(&state, Address::ZERO, 1_000_000_000, 1_000_000_000).await;

        let auction = auction_json("sell", "1000000000", "900000000", "1");
        let result = post_solve(&app, &auction).await;

        assert_eq!(result["solutions"][0]["gas"], CUT_AT_PARITY);
    }

    /// An `Active` proposal with no simulated gas cannot be priced. Reachable:
    /// `AcceptAll` activates without simulating.
    #[ignore]
    #[tokio::test]
    async fn solve_skips_a_proposal_that_has_not_been_simulated() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        let mut proposal =
            test_proposal(OrderUid(ORDER_UID), Address::ZERO, ProposalStatus::Active);
        proposal.sell_amount = U256::from(1_000_000_000u64);
        proposal.buy_amount = U256::from(1_000_000_000u64);
        proposal.gas_used = None;
        proposal.trampoline = Some(Address::ZERO);
        state.store().insert(proposal).await.expect("insert");

        let auction = auction_json("sell", "1000000000", "900000000", "1");
        let result = post_solve(&app, &auction).await;

        assert!(result["solutions"].as_array().unwrap().is_empty());
    }

    /// Without a price for the surplus token there is no score to compare, and
    /// the auction that omitted it could not rank our bid either.
    #[ignore]
    #[tokio::test]
    async fn solve_does_not_bid_when_the_surplus_token_is_unpriced() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        insert_active_proposal(&state, Address::ZERO, 1_000_000_000, 1_000_000_000).await;

        // Sell order: the surplus is in the buy token.
        let mut auction = auction_json("sell", "1000000000", "900000000", "1");
        auction["tokens"][BUY_TOKEN.to_string()]["referencePrice"] = serde_json::Value::Null;
        let result = post_solve(&app, &auction).await;

        assert!(result["solutions"].as_array().unwrap().is_empty());
    }

    /// The cut is in the sell token, so an unpriced sell token cannot be cut,
    /// and bidding without one means paying the gas ourselves.
    #[ignore]
    #[tokio::test]
    async fn solve_does_not_bid_when_the_sell_token_is_unpriced() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        insert_active_proposal(&state, Address::ZERO, 1_000_000_000, 1_000_000_000).await;

        let mut auction = auction_json("sell", "1000000000", "900000000", "1");
        auction["tokens"][SELL_TOKEN.to_string()]["referencePrice"] = serde_json::Value::Null;
        let result = post_solve(&app, &auction).await;

        assert!(
            result["solutions"].as_array().unwrap().is_empty(),
            "the surplus token is still priced, so only the missing cut can stop this bid",
        );
    }

    /// The `score > 0` boundary lives here and in the ingestion gate's
    /// `score <= min_score`, so it can drift. Gas equal to surplus scores zero,
    /// and zero does not bid.
    #[ignore]
    #[tokio::test]
    async fn solve_does_not_bid_when_gas_exactly_equals_the_surplus() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        // Surplus at parity pricing is 230_000 wei, the same as the gas bill.
        insert_active_proposal(&state, Address::ZERO, 1_000_000_000, 900_230_000).await;

        let auction = auction_json("sell", "1000000000", "900000000", "1");
        let result = post_solve(&app, &auction).await;

        assert!(result["solutions"].as_array().unwrap().is_empty());
    }

    #[ignore]
    #[tokio::test]
    async fn solve_selects_best_of_n_proposals() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);

        // Two proposals for the same order; second has more surplus.
        insert_active_proposal(&state, Address::ZERO, 1_000, 920).await;
        insert_active_proposal(&state, Address::ZERO, 1_000, 950).await;

        let auction = auction_json("sell", "1000", "900", "0");
        let result = post_solve(&app, &auction).await;

        let solutions = result["solutions"].as_array().unwrap();
        assert_eq!(solutions.len(), 1);
        // Best proposal has buy_amount=950, which becomes the sell_token price.
        assert_eq!(solutions[0]["prices"][SELL_TOKEN.to_string()], "950");
    }

    #[ignore]
    #[tokio::test]
    async fn solve_records_the_winning_solution_for_notify_attribution() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        insert_active_proposal(&state, Address::ZERO, 1_000, 950).await;

        let mut auction = auction_json("sell", "1000", "900", "0");
        auction["id"] = serde_json::json!("77");
        let result = post_solve(&app, &auction).await;
        assert_eq!(result["solutions"].as_array().unwrap().len(), 1);

        let attributed = state
            .store()
            .solution_proposals(77, &[1])
            .await
            .expect("solutions lookup");
        assert_eq!(
            attributed.len(),
            1,
            "a returned solution must be attributable via the solutions table"
        );
        assert_eq!(attributed[0].order_uid, OrderUid(ORDER_UID));
    }

    /// Acceptance: an `Executing` proposal is frozen out of
    /// `/solve` — its balances are about to be consumed by the in-flight
    /// settlement.
    #[ignore]
    #[tokio::test]
    async fn solve_never_offers_an_executing_proposal() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        let mut proposal = test_proposal(
            OrderUid(ORDER_UID),
            Address::ZERO,
            ProposalStatus::Executing,
        );
        proposal.gas_used = Some(200_000);
        proposal.trampoline = Some(Address::ZERO);
        state.store().insert(proposal).await.expect("insert");

        let auction = auction_json("sell", "1000", "900", "0");
        let result = post_solve(&app, &auction).await;

        assert!(result["solutions"].as_array().unwrap().is_empty());
    }

    #[ignore]
    #[tokio::test]
    async fn solve_no_proposals_returns_empty() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        // No proposals inserted.

        let auction = auction_json("sell", "1000", "900", "0");
        let result = post_solve(&app, &auction).await;

        let solutions = result["solutions"].as_array().unwrap();
        assert!(solutions.is_empty());
    }

    // -----------------------------------------------------------------------
    // Proposals carrying a real route
    // -----------------------------------------------------------------------

    /// Every other ingestion test posts an empty `interactions` array, which
    /// leaves `dto::interaction` unexercised even though every real proposal
    /// carries a route. The signature covers the interactions hash, so an
    /// interaction that survives the round trip byte-for-byte is also evidence
    /// that recovery ran over the same list the sub-solver signed.
    #[ignore]
    #[tokio::test]
    async fn a_proposal_carrying_a_route_is_accepted_and_stored_intact() {
        let state = test_state().await;
        let app = public_router(state.clone());
        let signer = PrivateKeySigner::random();
        let route = sample_route();
        let (body, sub_solver) = signed_proposal_body_for(&signer, &route).await;

        let response = post_proposal(&app, &body).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let id = json_body(response).await["id"].as_u64().expect("id");

        let stored = state
            .store()
            .get(crate::domain::proposal::ProposalId(id))
            .await
            .expect("get succeeds")
            .expect("the proposal must be stored");

        assert_eq!(stored.sub_solver, sub_solver);
        assert_eq!(
            stored.interactions, route,
            "the stored route must match the signed one exactly",
        );
        assert_eq!(
            stored.interactions_hash,
            eip712::compute_interactions_hash(&route),
            "the hash recovery ran over must be the hash of this route",
        );
    }

    /// `dto::interaction` reports its two parse failures separately, so a
    /// sub-solver debugging a malformed route learns which field to fix.
    #[ignore]
    #[tokio::test]
    async fn a_route_with_a_non_decimal_value_is_rejected() {
        let state = test_state().await;
        let app = public_router(state);
        let signer = PrivateKeySigner::random();
        let (mut body, _) = signed_proposal_body_for(&signer, &sample_route()).await;
        body["interactions"][0]["value"] = serde_json::json!("0x10");

        let response = post_proposal(&app, &body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["kind"], "BadRequest");
        assert_eq!(json["description"], "invalid interaction value");
    }

    #[ignore]
    #[tokio::test]
    async fn a_route_with_malformed_call_data_is_rejected() {
        let state = test_state().await;
        let app = public_router(state);
        let signer = PrivateKeySigner::random();
        let (mut body, _) = signed_proposal_body_for(&signer, &sample_route()).await;
        // Odd digit count — never a whole number of bytes.
        body["interactions"][1]["callData"] = serde_json::json!("0xabc");

        let response = post_proposal(&app, &body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["kind"], "BadRequest");
        assert_eq!(json["description"], "invalid interaction callData");
    }

    /// Pulls the `callData` bytes out of a serialized custom interaction.
    fn calldata_of(interaction: &serde_json::Value) -> Vec<u8> {
        let hex = interaction["callData"]
            .as_str()
            .expect("callData must be a hex string");
        alloy::hex::decode(hex.strip_prefix("0x").unwrap_or(hex)).expect("callData must be hex")
    }

    /// The encoder itself is covered in `byos-common`; what was not covered is
    /// the `build_solution` call site feeding it. Swapping its two token
    /// arguments still yields a well-formed solution, so decode both
    /// intra-interactions and check the tokens landed the right way round.
    #[ignore]
    #[tokio::test]
    async fn a_solution_wraps_the_route_in_the_two_trampoline_interactions() {
        use alloy::sol_types::SolCall;

        let state = test_state().await;
        let app = internal_router(state.clone(), None);
        let route = sample_route();
        let proposal = insert_routed_proposal(&state, Address::ZERO, &route).await;

        let result = post_solve(&app, &auction_json("sell", "1000", "900", "0")).await;
        let interactions = result["solutions"][0]["interactions"]
            .as_array()
            .expect("a solution must carry interactions");
        assert_eq!(
            interactions.len(),
            2,
            "transfer then execute, and nothing else (contracts ADR-0003)",
        );

        // 1. sellToken.transfer(trampoline, sellAmount)
        assert_eq!(interactions[0]["target"], SELL_TOKEN.to_string());
        assert_eq!(
            interactions[0]["internalize"], false,
            "internalizing would skip the transfer that funds the route",
        );
        let transfer = contracts::ERC20::transferCall::abi_decode(&calldata_of(&interactions[0]))
            .expect("the first interaction must decode as transfer()");
        assert_eq!(
            transfer.to, TRAMPOLINE,
            "trade capital must go to this sub-solver's trampoline",
        );
        assert_eq!(transfer.amount, proposal.sell_amount);

        // 2. trampoline.execute(proposal, route, sellToken, buyToken, signature)
        assert_eq!(interactions[1]["target"], TRAMPOLINE.to_string());
        let execute =
            contracts::Trampoline::executeCall::abi_decode(&calldata_of(&interactions[1]))
                .expect("the second interaction must decode as execute()");
        assert_eq!(
            execute._sellToken, SELL_TOKEN,
            "the order's sell token must not arrive as the buy token",
        );
        assert_eq!(
            execute._buyToken, BUY_TOKEN,
            "the order's buy token must not arrive as the sell token",
        );
        assert_eq!(
            execute._interactions, route,
            "the sub-solver's route must reach the trampoline unchanged",
        );
        assert_eq!(execute._signature, proposal.signature);
    }

    const HOOKS_TRAMPOLINE: Address = address!("6666666666666666666666666666666666666666");

    /// Creates an AppState with `hooks_trampoline` set, so `/solve` encodes
    /// hooks into the solution's pre/post interactions.
    async fn test_state_with_hooks_trampoline() -> AppState {
        let (audit_tx, audit_rx) = tokio::sync::mpsc::unbounded_channel();
        std::mem::forget(audit_rx);
        let db = crate::tests::setup::TestDb::create().await;
        let pool = crate::infra::audit::connect_and_migrate(&db.url)
            .await
            .expect("migrations run");
        let domain = eip712::byos_domain(CHAIN_ID, factory());
        let gas_price = Arc::new(AtomicU64::new(0));
        AppState::new(
            Arc::new(ProposalStore::new(pool, audit_tx)),
            domain,
            gas_price,
            300,
            Some(HOOKS_TRAMPOLINE),
        )
    }

    /// A hooked order's pre/post interactions must appear in the `/solve`
    /// response as `preInteractions` / `postInteractions`, encoded as
    /// `HooksTrampoline.execute(hooks)` calls.
    #[ignore]
    #[tokio::test]
    async fn solve_includes_hook_interactions_for_a_hooked_order() {
        use alloy::sol_types::SolCall;

        let state = test_state_with_hooks_trampoline().await;
        let app = internal_router(state.clone(), None);

        let pre_hook = byos_common::hooks::Hook {
            target: address!("000000000000000000000000000000000000aaaa"),
            call_data: alloy::primitives::Bytes::from(vec![0xab, 0xcd]),
            gas_limit: U256::from(100_000_u64),
        };
        let post_hook = byos_common::hooks::Hook {
            target: address!("000000000000000000000000000000000000bbbb"),
            call_data: alloy::primitives::Bytes::from(vec![0xef]),
            gas_limit: U256::from(50_000_u64),
        };

        let mut proposal =
            test_proposal(OrderUid(ORDER_UID), Address::ZERO, ProposalStatus::Active);
        proposal.sell_amount = U256::from(1_000_u64);
        proposal.buy_amount = U256::from(950_u64);
        proposal.gas_used = Some(200_000);
        proposal.trampoline = Some(TRAMPOLINE);
        proposal.hooks = byos_common::hooks::Hooks {
            pre: vec![pre_hook.clone()],
            post: vec![post_hook.clone()],
        };
        state
            .store()
            .insert(proposal)
            .await
            .expect("insert succeeds");

        let result = post_solve(&app, &auction_json("sell", "1000", "900", "0")).await;

        let solution = &result["solutions"][0];

        // Pre-interactions: one HooksTrampoline.execute([pre_hook]) call.
        let pre = solution["preInteractions"]
            .as_array()
            .expect("preInteractions must be an array");
        assert_eq!(pre.len(), 1, "one pre-hook → one HooksTrampoline call");
        assert_eq!(
            pre[0]["target"]
                .as_str()
                .unwrap()
                .to_lowercase(),
            format!("{HOOKS_TRAMPOLINE:#x}"),
        );
        let pre_execute = byos_common::contracts::HooksTrampoline::executeCall::abi_decode(
            &calldata_of(&pre[0]),
        )
        .expect("pre-interaction must decode as HooksTrampoline.execute()");
        assert_eq!(pre_execute.hooks.len(), 1);
        assert_eq!(pre_execute.hooks[0].target, pre_hook.target);
        assert_eq!(pre_execute.hooks[0].callData, pre_hook.call_data);
        assert_eq!(pre_execute.hooks[0].gasLimit, pre_hook.gas_limit);

        // Post-interactions: one HooksTrampoline.execute([post_hook]) call.
        let post = solution["postInteractions"]
            .as_array()
            .expect("postInteractions must be an array");
        assert_eq!(post.len(), 1, "one post-hook → one HooksTrampoline call");
        assert_eq!(
            post[0]["target"]
                .as_str()
                .unwrap()
                .to_lowercase(),
            format!("{HOOKS_TRAMPOLINE:#x}"),
        );
        let post_execute = byos_common::contracts::HooksTrampoline::executeCall::abi_decode(
            &calldata_of(&post[0]),
        )
        .expect("post-interaction must decode as HooksTrampoline.execute()");
        assert_eq!(post_execute.hooks.len(), 1);
        assert_eq!(post_execute.hooks[0].target, post_hook.target);
    }

    /// A proposal only reaches `/solve` unresolved if the validator accepted it
    /// without deriving the CREATE2 trampoline address. That would be a bug
    /// upstream, but it has to cost us the bid rather than emit a solution
    /// whose transfer goes nowhere.
    #[ignore]
    #[tokio::test]
    async fn a_proposal_without_a_resolved_trampoline_is_not_offered() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);

        // Amounts, gas and status all match a proposal that does get bid, so
        // the missing trampoline is the only thing that can explain the empty
        // result.
        let mut proposal =
            test_proposal(OrderUid(ORDER_UID), Address::ZERO, ProposalStatus::Active);
        proposal.sell_amount = U256::from(1_000_u64);
        proposal.buy_amount = U256::from(950_u64);
        proposal.gas_used = Some(200_000);
        proposal.trampoline = None;
        state
            .store()
            .insert(proposal)
            .await
            .expect("insert succeeds");

        let result = post_solve(&app, &auction_json("sell", "1000", "900", "0")).await;

        assert!(
            result["solutions"]
                .as_array()
                .expect("solutions is an array")
                .is_empty(),
            "a proposal with no trampoline must not be bid",
        );
    }

    // -----------------------------------------------------------------------
    // Auctions carrying more than one order
    // -----------------------------------------------------------------------

    const SECOND_ORDER_UID: [u8; 56] = [0xbb; 56];

    /// Two sell orders on the same token pair, differing only in UID and
    /// amounts, so a test can tell the resulting solutions apart.
    fn two_order_auction_json() -> serde_json::Value {
        let mut auction = auction_json("sell", "1000", "900", "0");
        let mut second = auction["orders"][0].clone();
        second["uid"] = serde_json::json!(format!("0x{}", alloy::hex::encode(SECOND_ORDER_UID)));
        second["sellAmount"] = serde_json::json!("2000");
        second["fullSellAmount"] = serde_json::json!("2000");
        second["buyAmount"] = serde_json::json!("1800");
        second["fullBuyAmount"] = serde_json::json!("1800");
        auction["orders"] = serde_json::json!([auction["orders"][0].clone(), second]);
        auction
    }

    /// Every other `/solve` test uses a single-order auction, which cannot show
    /// that orders are scored independently or that solution ids count up.
    /// The driver keys its `/notify` attribution on those ids (ADR-0013), so a
    /// duplicate would misattribute a settlement.
    #[ignore]
    #[tokio::test]
    async fn each_order_in_an_auction_gets_its_own_numbered_solution() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);

        insert_active_proposal(&state, Address::ZERO, 1_000, 950).await;
        let mut second = test_proposal(
            OrderUid(SECOND_ORDER_UID),
            Address::ZERO,
            ProposalStatus::Active,
        );
        second.sell_amount = U256::from(2_000_u64);
        second.buy_amount = U256::from(1_900_u64);
        second.gas_used = Some(200_000);
        second.trampoline = Some(TRAMPOLINE);
        state.store().insert(second).await.expect("insert succeeds");

        let result = post_solve(&app, &two_order_auction_json()).await;

        let solutions = result["solutions"]
            .as_array()
            .expect("solutions is an array");
        assert_eq!(solutions.len(), 2, "each order must be scored on its own");
        assert_eq!(
            solutions
                .iter()
                .map(|s| s["id"].as_u64())
                .collect::<Vec<_>>(),
            vec![Some(1), Some(2)],
            "ids must be distinct — the driver attributes notifications by them",
        );

        // Each solution must carry the amounts of its own order, not the other's.
        assert_eq!(solutions[0]["trades"][0]["executedAmount"], "1000");
        assert_eq!(solutions[1]["trades"][0]["executedAmount"], "2000");
    }

    // -----------------------------------------------------------------------
    // Auction gas price published to the escrow validator
    // -----------------------------------------------------------------------

    /// The background escrow validator reads this cell instead of the
    /// `--default-gas-price` startup fallback, so publishing it here is what
    /// keeps the escrow threshold tracking a live gas price.
    #[ignore]
    #[tokio::test]
    async fn solve_publishes_the_auction_gas_price_for_the_escrow_validator() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);

        let auction = auction_json("sell", "1000", "900", "7000000000");
        post_solve(&app, &auction).await;

        assert_eq!(
            state.gas_price().load(Ordering::Relaxed),
            7_000_000_000,
            "the auction's effective gas price must reach the shared cell",
        );
    }

    /// Saturating to `u64::MAX` here would push every sub-solver under the
    /// escrow threshold and reject the entire live book on the next validation
    /// tick, and `Rejected` is terminal (ADR-0013). So an unrepresentable
    /// price has to leave the last good one standing.
    #[ignore]
    #[tokio::test]
    async fn a_gas_price_that_does_not_fit_u64_leaves_the_previous_one_in_place() {
        let state = test_state().await;
        let app = internal_router(state.clone(), None);

        post_solve(&app, &auction_json("sell", "1000", "900", "7000000000")).await;

        // 2^64: one past what the cell can hold.
        let oversized = auction_json("sell", "1000", "900", "18446744073709551616");
        post_solve(&app, &oversized).await;

        assert_eq!(
            state.gas_price().load(Ordering::Relaxed),
            7_000_000_000,
            "an unrepresentable price must not disturb the last good one",
        );
    }

    // -----------------------------------------------------------------------
    // Ingestion-time expiry check
    // -----------------------------------------------------------------------

    #[ignore]
    #[tokio::test]
    async fn post_rejects_already_expired_proposal() {
        let state = test_state().await;
        let app = public_router(state);
        let signer = PrivateKeySigner::random();
        let domain = eip712::byos_domain(CHAIN_ID, factory());

        let order_uid = [0xaa_u8; 56];
        let proposal = contracts::Proposal {
            orderUidHash: keccak256(order_uid),
            sellAmount: U256::from(1_000_000_u64),
            buyAmount: U256::from(990_000_u64),
            validUntil: U256::from(1_u64), // unix timestamp 1 — long expired
            nonce: U256::from(1_u64),
        };
        let interactions: Vec<contracts::Interaction> = vec![];

        let signature = eip712::sign_proposal(&signer, &domain, &proposal, &interactions)
            .await
            .expect("signing must succeed");

        let body = serde_json::json!({
            "orderUid": format!("0x{}", alloy::hex::encode(order_uid)),
            "sellAmount": "1000000",
            "buyAmount": "990000",
            "interactions": [],
            "validUntil": "1",
            "nonce": "1",
            "signature": format!("0x{}", alloy::hex::encode(signature.as_bytes())),
        });

        let response = post_proposal(&app, &body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let json = json_body(response).await;
        assert_eq!(json["kind"], "ProposalExpired");
    }
}
