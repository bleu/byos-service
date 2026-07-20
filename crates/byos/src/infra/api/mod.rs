//! Public API: proposal CRUD endpoints and health check.

pub mod dto;
pub mod error;
pub mod routes;
pub mod solve;

use {
    crate::domain::proposal::InMemoryProposalStore,
    alloy::sol_types::Eip712Domain,
    axum::{
        Router,
        routing::{delete, get, post},
    },
    std::{net::SocketAddr, sync::Arc},
    tokio::sync::oneshot,
};

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

struct AppStateInner {
    store: Arc<InMemoryProposalStore>,
    domain: Eip712Domain,
}

/// Shared application state, cheaply cloneable via `Arc`. The store is
/// separately `Arc`ed because the background validation loop shares it.
#[derive(Clone)]
pub struct AppState(Arc<AppStateInner>);

impl AppState {
    pub fn new(store: Arc<InMemoryProposalStore>, domain: Eip712Domain) -> Self {
        Self(Arc::new(AppStateInner { store, domain }))
    }

    pub fn store(&self) -> &InMemoryProposalStore {
        &self.0.store
    }

    pub fn domain(&self) -> &Eip712Domain {
        &self.0.domain
    }
}

// ---------------------------------------------------------------------------
// Router + serve
// ---------------------------------------------------------------------------

fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(routes::healthz))
        .route("/proposals", post(routes::create_proposal))
        .route("/proposal/{id}", get(routes::get_proposal))
        .route("/proposal/{id}", delete(routes::cancel_proposal))
        .route("/proposals/{order_uid}", get(routes::list_proposals))
        .route(
            "/proposals/by-solver",
            get(routes::list_proposals_by_solver),
        )
        .route("/solve", post(solve::solve))
        .with_state(state)
}

/// Typed error for the public API server (ADR-0007: library functions avoid
/// `anyhow::Result`; callers can match on failure modes).
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("failed to bind listener")]
    Bind(#[source] std::io::Error),
    #[error("server error")]
    Serve(#[source] std::io::Error),
}

/// Bind, serve, and wait for graceful shutdown (ctrl-c, or `shutdown_rx` so
/// tests can stop an in-process instance).
pub async fn serve(
    addr: SocketAddr,
    state: AppState,
    bind_tx: Option<oneshot::Sender<SocketAddr>>,
    shutdown_rx: Option<oneshot::Receiver<()>>,
) -> Result<(), ServeError> {
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(ServeError::Bind)?;
    let local_addr = listener.local_addr().map_err(ServeError::Bind)?;

    tracing::info!(port = local_addr.port(), "serving public API");

    if let Some(tx) = bind_tx {
        let _ = tx.send(local_addr);
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_rx))
        .await
        .map_err(ServeError::Serve)?;

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
        tower::ServiceExt,
    };

    const CHAIN_ID: u64 = 1;

    fn factory() -> Address {
        Address::repeat_byte(0x42)
    }

    fn test_state() -> AppState {
        // These router tests assert on HTTP behaviour, not audit evidence.
        // Leaking the receiver keeps the channel open so emits stay silent.
        let (audit_tx, audit_rx) = tokio::sync::mpsc::unbounded_channel();
        std::mem::forget(audit_rx);
        let domain = eip712::byos_domain(CHAIN_ID, factory());
        AppState::new(Arc::new(InMemoryProposalStore::new(audit_tx)), domain)
    }

    /// Builds a valid signed POST /proposals JSON body and returns it along
    /// with the signer's address.
    async fn signed_proposal_body_for(signer: &PrivateKeySigner) -> (serde_json::Value, Address) {
        let domain = eip712::byos_domain(CHAIN_ID, factory());

        let order_uid = [0xaa_u8; 56];
        let proposal = contracts::Proposal {
            orderUidHash: keccak256(order_uid),
            sellAmount: U256::from(1_000_000_u64),
            buyAmount: U256::from(990_000_u64),
            validUntil: U256::from(99_999_999_999_u64),
            nonce: U256::from(1_u64),
        };
        let interactions: Vec<contracts::Interaction> = vec![];

        let signature = eip712::sign_proposal(signer, &domain, &proposal, &interactions)
            .await
            .expect("signing must succeed");

        let body = serde_json::json!({
            "orderUid": format!("0x{}", alloy::hex::encode(order_uid)),
            "sellAmount": "1000000",
            "buyAmount": "990000",
            "interactions": [],
            "validUntil": "99999999999",
            "nonce": "1",
            "signature": format!("0x{}", alloy::hex::encode(signature.as_bytes())),
        });
        (body, signer.address())
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

    struct RejectAll;

    impl crate::domain::validator::ProposalValidator for RejectAll {
        async fn validate(
            &self,
            _proposal: &crate::domain::proposal::Proposal,
        ) -> crate::domain::validator::Verdict {
            crate::domain::validator::Verdict::Reject(
                crate::domain::validator::RejectionReason::InsufficientEscrow,
            )
        }
    }

    #[tokio::test]
    async fn rejected_proposal_exposes_reason_on_the_wire() {
        let state = test_state();
        let app = router(state.clone());
        let signer = PrivateKeySigner::random();
        let (body, _) = signed_proposal_body_for(&signer).await;

        let response = post_proposal(&app, &body).await;
        let id = json_body(response).await["id"].as_u64().expect("id");

        crate::infra::validation::run_tick(state.store(), &RejectAll, 0).await;

        let header = read_auth_header(&signer, &state).await;
        let (status, body) = get(state, &format!("/proposal/{id}"), Some(&header)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "rejected");
        assert_eq!(body["rejectionReason"], "InsufficientEscrow");
    }

    #[tokio::test]
    async fn double_cancel_returns_conflict() {
        use alloy::sol_types::SolStruct;

        let domain = eip712::byos_domain(CHAIN_ID, factory());
        let state = test_state();
        let app = router(state);

        let signer = PrivateKeySigner::random();
        let (body, _) = signed_proposal_body_for(&signer).await;
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

    #[tokio::test]
    async fn post_returns_202_and_proposal_is_submitted() {
        let state = test_state();
        let app = router(state.clone());
        let signer = PrivateKeySigner::random();
        let (body, _) = signed_proposal_body_for(&signer).await;

        let response = post_proposal(&app, &body).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let id = json_body(response).await["id"].as_u64().expect("id");

        let header = read_auth_header(&signer, &state).await;
        let (status, json) = get(state, &format!("/proposal/{id}"), Some(&header)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["status"], "submitted");
    }

    fn insert_proposal(
        state: &AppState,
        sub_solver: Address,
    ) -> crate::domain::proposal::ProposalId {
        state.store().insert(test_proposal(
            OrderUid([0xaa; 56]),
            sub_solver,
            ProposalStatus::Active,
        ))
    }

    /// Signs the `ReadAuth` bearer message and formats it for `X-Signature`.
    async fn read_auth_header(signer: &PrivateKeySigner, state: &AppState) -> String {
        let sig = byos_common::eip712::sign_read_auth(signer, state.domain())
            .await
            .expect("signing should succeed");
        format!("0x{}", alloy::hex::encode(sig.as_bytes()))
    }

    /// Fires a GET at the router, optionally with an `X-Signature` header.
    /// Returns the status and parsed JSON body.
    async fn get(
        state: AppState,
        uri: &str,
        signature: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder().uri(uri);
        if let Some(sig) = signature {
            request = request.header("X-Signature", sig);
        }
        let response = router(state)
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

    #[tokio::test]
    async fn get_proposal_owner_reads_own() {
        let state = test_state();
        let owner = alloy::signers::local::PrivateKeySigner::random();
        let id = insert_proposal(&state, owner.address());
        let header = read_auth_header(&owner, &state).await;

        let (status, json) = get(state, &format!("/proposal/{id}"), Some(&header)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["id"], id.0);
        assert_eq!(json["sellAmount"], "1000000");
        assert_eq!(json["buyAmount"], "990000");
    }

    #[tokio::test]
    async fn get_proposal_non_owner_gets_404() {
        let state = test_state();
        let owner = address!("0000000000000000000000000000000000000001");
        let id = insert_proposal(&state, owner);

        let other = alloy::signers::local::PrivateKeySigner::random();
        let header = read_auth_header(&other, &state).await;

        let (status, _) = get(state, &format!("/proposal/{id}"), Some(&header)).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn list_by_order_uid_scoped_to_caller() {
        let state = test_state();
        let caller = alloy::signers::local::PrivateKeySigner::random();
        let competitor = address!("0000000000000000000000000000000000000002");

        // Two proposals on the same order UID, different sub-solvers.
        insert_proposal(&state, caller.address());
        insert_proposal(&state, competitor);

        let header = read_auth_header(&caller, &state).await;
        let uid_hex = format!("0x{}", alloy::hex::encode([0xaa; 56]));

        let (status, json) = get(state, &format!("/proposals/{uid_hex}"), Some(&header)).await;

        assert_eq!(status, StatusCode::OK);
        let proposals = json["proposals"].as_array().unwrap();
        assert_eq!(proposals.len(), 1, "competitor's proposal must not leak");
        let returned: Address = proposals[0]["subSolver"].as_str().unwrap().parse().unwrap();
        assert_eq!(returned, caller.address());
    }

    #[tokio::test]
    async fn list_by_solver_uses_signer_identity() {
        let state = test_state();
        let caller = alloy::signers::local::PrivateKeySigner::random();
        let competitor = address!("0000000000000000000000000000000000000002");

        insert_proposal(&state, caller.address());
        insert_proposal(&state, competitor);

        let header = read_auth_header(&caller, &state).await;

        let (status, json) = get(state, "/proposals/by-solver", Some(&header)).await;

        assert_eq!(status, StatusCode::OK);
        let proposals = json["proposals"].as_array().unwrap();
        assert_eq!(proposals.len(), 1);
        let returned: Address = proposals[0]["subSolver"].as_str().unwrap().parse().unwrap();
        assert_eq!(returned, caller.address());
    }

    #[tokio::test]
    async fn get_proposal_without_signature_is_rejected() {
        let state = test_state();
        let solver = address!("0000000000000000000000000000000000000001");
        let id = insert_proposal(&state, solver);

        let (status, _) = get(state, &format!("/proposal/{id}"), None).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    // -----------------------------------------------------------------------
    // /solve tests
    // -----------------------------------------------------------------------

    const SELL_TOKEN: Address = address!("1111111111111111111111111111111111111111");
    const BUY_TOKEN: Address = address!("2222222222222222222222222222222222222222");
    const ORDER_UID: [u8; 56] = [0xaa; 56];

    /// Builds a minimal valid auction JSON with one order.
    fn auction_json(kind: &str, sell_amount: &str, buy_amount: &str) -> serde_json::Value {
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
            "effectiveGasPrice": "0",
            "deadline": "2099-01-01T00:00:00Z",
            "surplusCapturingJitOrderOwners": []
        })
    }

    fn insert_active_proposal(
        state: &AppState,
        sub_solver: Address,
        sell_amount: u64,
        buy_amount: u64,
    ) {
        let mut proposal = test_proposal(
            OrderUid(ORDER_UID),
            sub_solver,
            ProposalStatus::Active,
        );
        proposal.sell_amount = U256::from(sell_amount);
        proposal.buy_amount = U256::from(buy_amount);
        state.store().insert(proposal);
    }

    async fn post_solve(app: &Router, auction: &serde_json::Value) -> serde_json::Value {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/solve")
                    .header("content-type", "application/json")
                    .body(Body::from(auction.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        json_body(response).await
    }

    #[tokio::test]
    async fn solve_sell_order_prices_are_cross_multiplied() {
        let state = test_state();
        let app = router(state.clone());
        insert_active_proposal(&state, Address::ZERO, 1_000, 950);

        let auction = auction_json("sell", "1000", "900");
        let result = post_solve(&app, &auction).await;

        let solutions = result["solutions"].as_array().unwrap();
        assert_eq!(solutions.len(), 1);

        let prices = &solutions[0]["prices"];
        // sell_token price = proposal.buy_amount, buy_token price = proposal.sell_amount
        assert_eq!(prices[SELL_TOKEN.to_string()], "950");
        assert_eq!(prices[BUY_TOKEN.to_string()], "1000");
    }

    #[tokio::test]
    async fn solve_sell_order_executed_amount_is_sell() {
        let state = test_state();
        let app = router(state.clone());
        insert_active_proposal(&state, Address::ZERO, 1_000, 950);

        let auction = auction_json("sell", "1000", "900");
        let result = post_solve(&app, &auction).await;

        let trade = &result["solutions"][0]["trades"][0];
        assert_eq!(trade["executedAmount"], "1000");
    }

    #[tokio::test]
    async fn solve_buy_order_executed_amount_is_buy() {
        let state = test_state();
        let app = router(state.clone());
        insert_active_proposal(&state, Address::ZERO, 950, 900);

        let auction = auction_json("buy", "1000", "900");
        let result = post_solve(&app, &auction).await;

        let trade = &result["solutions"][0]["trades"][0];
        assert_eq!(trade["executedAmount"], "900");
    }

    #[tokio::test]
    async fn solve_selects_best_of_n_proposals() {
        let state = test_state();
        let app = router(state.clone());

        // Two proposals for the same order; second has more surplus.
        insert_active_proposal(&state, Address::ZERO, 1_000, 920);
        insert_active_proposal(&state, Address::ZERO, 1_000, 950);

        let auction = auction_json("sell", "1000", "900");
        let result = post_solve(&app, &auction).await;

        let solutions = result["solutions"].as_array().unwrap();
        assert_eq!(solutions.len(), 1);
        // Best proposal has buy_amount=950, which becomes the sell_token price.
        assert_eq!(solutions[0]["prices"][SELL_TOKEN.to_string()], "950");
    }

    #[tokio::test]
    async fn solve_no_proposals_returns_empty() {
        let state = test_state();
        let app = router(state.clone());
        // No proposals inserted.

        let auction = auction_json("sell", "1000", "900");
        let result = post_solve(&app, &auction).await;

        let solutions = result["solutions"].as_array().unwrap();
        assert!(solutions.is_empty());
    }
}
