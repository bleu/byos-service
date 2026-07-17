//! Startup: parse CLI args → init tracing → build app → serve with graceful
//! shutdown. The `run()` variant accepts a `oneshot::Sender<SocketAddr>` so
//! e2e tests can discover the bound port.

use {
    crate::{
        domain::proposal::InMemoryProposalStore,
        infra::api::{self, AppState},
    },
    anyhow::Context,
    clap::Parser,
    std::net::SocketAddr,
    tokio::sync::oneshot,
    tracing_subscriber::{EnvFilter, fmt, prelude::*},
};

/// CLI args. Each flag doubles as an env var (ADR-0006).
#[derive(Parser, Debug)]
#[command(version)]
pub(crate) struct Args {
    /// Log filter directive (e.g. `warn,byos=debug`).
    #[arg(long, env, default_value = "warn,byos=debug")]
    log: String,

    /// Emit JSON-formatted logs.
    #[arg(long, env, default_value_t = false)]
    json_logs: bool,

    /// Public API listener address (proposals endpoints).
    #[arg(long, env, default_value = "0.0.0.0:8080")]
    public_addr: SocketAddr,

    /// Chain ID for the EIP-712 domain.
    #[arg(long, env)]
    chain_id: u64,

    /// TrampolineFactory contract address (EIP-712 `verifyingContract`).
    #[arg(long, env)]
    trampoline_factory: alloy::primitives::Address,
}

/// Entry point for the binary — parses args from the process environment.
pub async fn start(args: impl IntoIterator<Item = String>) {
    let args = Args::parse_from(args);
    if let Err(e) = run_with(args, None).await {
        eprintln!("fatal: {e:#}");
        std::process::exit(1);
    }
}

/// Entry point for tests — caller passes args and receives the bound address.
pub async fn run(
    args: impl IntoIterator<Item = String>,
    bind_tx: oneshot::Sender<SocketAddr>,
) -> anyhow::Result<()> {
    let args = Args::parse_from(args);
    run_with(args, Some(bind_tx)).await
}

async fn run_with(args: Args, bind_tx: Option<oneshot::Sender<SocketAddr>>) -> anyhow::Result<()> {
    init_tracing(&args.log, args.json_logs);

    tracing::info!(?args, "starting byos");

    let domain = byos_common::eip712::byos_domain(args.chain_id, args.trampoline_factory);
    let store = InMemoryProposalStore::new();
    let state = AppState::new(store, domain);

    api::serve(args.public_addr, state, bind_tx)
        .await
        .context("public API server exited with error")
}

fn init_tracing(filter: &str, json: bool) {
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("warn"));

    // `try_init` rather than `init`: service-level tests call `run()` once per
    // test, and under plain `cargo test` (shared process, unlike nextest) the
    // second init would panic. Only the first subscriber wins; that's fine.
    if json {
        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().json())
            .try_init();
    } else {
        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer())
            .try_init();
    }
}
