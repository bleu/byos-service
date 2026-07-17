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
            "/proposals/by-solver/{address}",
            get(routes::list_proposals_by_solver),
        )
        .with_state(state)
}

/// Bind, serve, and wait for graceful shutdown (ctrl-c, or `shutdown_rx` so
/// tests can stop an in-process instance).
pub async fn serve(
    addr: SocketAddr,
    state: AppState,
    bind_tx: Option<oneshot::Sender<SocketAddr>>,
    shutdown_rx: Option<oneshot::Receiver<()>>,
) -> anyhow::Result<()> {
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;

    tracing::info!(port = local_addr.port(), "serving public API");

    if let Some(tx) = bind_tx {
        let _ = tx.send(local_addr);
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_rx))
        .await?;

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
