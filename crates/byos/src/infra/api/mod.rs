//! Public API: proposal CRUD endpoints and health check.

pub mod dto;
pub mod error;
pub mod routes;

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
    store: InMemoryProposalStore,
    domain: Eip712Domain,
}

/// Shared application state, cheaply cloneable via `Arc`.
#[derive(Clone)]
pub struct AppState(Arc<AppStateInner>);

impl AppState {
    pub fn new(store: InMemoryProposalStore, domain: Eip712Domain) -> Self {
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
        .with_state(state)
}

/// Bind, serve, and wait for graceful shutdown.
pub async fn serve(
    addr: SocketAddr,
    state: AppState,
    bind_tx: Option<oneshot::Sender<SocketAddr>>,
) -> anyhow::Result<()> {
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;

    tracing::info!(port = local_addr.port(), "serving public API");

    if let Some(tx) = bind_tx {
        let _ = tx.send(local_addr);
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    ctrl_c.await.ok();
    tracing::info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::domain::proposal::{OrderUid, Proposal, ProposalStatus},
        alloy::primitives::{Address, B256, Bytes, U256, address, keccak256},
        axum::{
            body::Body,
            http::{Request, StatusCode},
        },
        std::time::Instant,
        tower::ServiceExt,
    };

    /// EIP-712 domain anchored to a dummy factory, matching what tests sign
    /// against.
    fn test_state() -> AppState {
        let domain = byos_common::eip712::byos_domain(
            1,
            address!("00000000000000000000000000000000DeaDBeef"),
        );
        AppState::new(
            crate::domain::proposal::InMemoryProposalStore::new(),
            domain,
        )
    }

    fn insert_proposal(state: &AppState, sub_solver: Address) -> u64 {
        let order_uid = OrderUid([0xaa; 56]);
        let order_uid_hash = keccak256(order_uid.0);
        state.store().insert(Proposal {
            id: 0,
            sub_solver,
            order_uid,
            order_uid_hash,
            sell_amount: U256::from(1_000_000u64),
            buy_amount: U256::from(990_000u64),
            interactions: vec![],
            interactions_hash: B256::ZERO,
            valid_until: U256::from(u64::MAX),
            nonce: U256::from(1u64),
            signature: Bytes::new(),
            status: ProposalStatus::Active,
            created_at: Instant::now(),
        })
    }

    /// Signs the `ReadAuth` bearer message and formats it for `X-Signature`.
    async fn read_auth_header(
        signer: &alloy::signers::local::PrivateKeySigner,
        state: &AppState,
    ) -> String {
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
        assert_eq!(json["id"], id);
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
}
