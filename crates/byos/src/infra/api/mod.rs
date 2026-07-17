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
            "/proposals/by-solver/{address}",
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
        alloy::{
            primitives::{Address, U256, keccak256},
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

    fn test_app() -> Router {
        let domain = eip712::byos_domain(CHAIN_ID, factory());
        let state = AppState::new(Arc::new(InMemoryProposalStore::new()), domain);
        router(state)
    }

    /// Builds a valid signed POST /proposals JSON body and returns it along
    /// with the signer's address.
    async fn signed_proposal_body() -> (serde_json::Value, Address) {
        signed_proposal_body_for(&PrivateKeySigner::random()).await
    }

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

    async fn get_proposal(app: &Router, id: u64) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/proposal/{id}"))
                    .body(Body::empty())
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
        let domain = eip712::byos_domain(CHAIN_ID, factory());
        let state = AppState::new(Arc::new(InMemoryProposalStore::new()), domain);
        let app = router(state.clone());
        let (body, _) = signed_proposal_body().await;

        let response = post_proposal(&app, &body).await;
        let id = json_body(response).await["id"].as_u64().expect("id");

        crate::infra::validation::run_tick(state.store(), &RejectAll, 0).await;

        let body = json_body(get_proposal(&app, id).await).await;
        assert_eq!(body["status"], "rejected");
        assert_eq!(body["rejectionReason"], "InsufficientEscrow");
    }

    #[tokio::test]
    async fn double_cancel_returns_conflict() {
        use alloy::sol_types::SolStruct;

        let domain = eip712::byos_domain(CHAIN_ID, factory());
        let state = AppState::new(Arc::new(InMemoryProposalStore::new()), domain.clone());
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
        let app = test_app();
        let (body, _) = signed_proposal_body().await;

        let response = post_proposal(&app, &body).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let id = json_body(response).await["id"].as_u64().expect("id");

        let response = get_proposal(&app, id).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["status"], "submitted");
    }
}
