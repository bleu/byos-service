//! Startup: parse CLI args → init tracing → build app → serve with graceful
//! shutdown. The `run()` variant accepts a `oneshot::Sender<SocketAddr>` so
//! e2e tests can discover the bound port.

use {
    crate::{
        domain::proposal::InMemoryProposalStore,
        infra::{
            api::{self, AppState},
            audit,
            blockchain::{
                escrow::EscrowValidator,
                validator::{ProposalValidator, SimulationValidator},
            },
        },
    },
    alloy::{primitives::U256, providers::Provider},
    anyhow::Context,
    clap::Parser,
    std::{
        net::SocketAddr,
        sync::{Arc, atomic::AtomicU64},
    },
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

    /// Postgres URL for the audit trail (ADR-0001 write-behind). Required:
    /// the service refuses to boot without its evidence store. Prefer the
    /// DATABASE_URL env var in production — CLI arguments (and the password
    /// in this one) are visible to other users via `ps`.
    #[arg(long, env)]
    database_url: DatabaseUrl,

    /// RPC endpoint for chain connectivity (escrow balance checks). When
    /// omitted the service starts with an AcceptAll validator (useful for
    /// tests that don't need chain connectivity). Prefer the RPC_URL env var
    /// in production — the URL may contain API keys. When set, requires
    /// `--escrow-address`, `--min-collateral`, and `--default-gas-price`.
    #[arg(long, env, requires_all = ["escrow_address", "min_collateral", "default_gas_price", "settlement_address"])]
    rpc_url: Option<RpcUrl>,

    /// Escrow contract address for sub-solver balance checks. Required when
    /// `--rpc-url` is set.
    #[arg(long, env)]
    escrow_address: Option<alloy::primitives::Address>,

    /// GPv2Settlement contract address. Used as both `from` and `to` for
    /// simulation `eth_estimateGas` calls. Required when `--rpc-url` is set.
    #[arg(long, env)]
    settlement_address: Option<alloy::primitives::Address>,

    /// Minimum collateral (`c_l`) in wei. Chain-specific: 0.010 ETH for
    /// mainnet (~10000000000000000), 10 xDAI for Gnosis
    /// (~10000000000000000000). Required when `--rpc-url` is set.
    #[arg(long, env)]
    min_collateral: Option<u128>,

    /// Fallback gas price in wei, used for the escrow threshold when no
    /// auction has been seen yet. Overwritten by `/solve` once the first
    /// auction arrives. Required when `--rpc-url` is set.
    #[arg(long, env)]
    default_gas_price: Option<u64>,

    /// Seconds between background validation ticks (expiry sweep + verdicts).
    #[arg(long, env, default_value_t = 12)]
    validation_interval_secs: u64,
}

/// Connection-string wrapper whose `Debug` hides the value, so the startup
/// `?args` log can't leak the password (ADR-0006: secrets redact themselves).
#[derive(Clone)]
struct DatabaseUrl(String);

/// RPC URL wrapper whose `Debug` hides the value — the URL may contain
/// API keys (ADR-0006: secrets redact themselves).
#[derive(Clone)]
struct RpcUrl(String);

impl std::str::FromStr for DatabaseUrl {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_owned()))
    }
}

impl std::fmt::Debug for DatabaseUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl std::str::FromStr for RpcUrl {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_owned()))
    }
}

impl std::fmt::Debug for RpcUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Entry point for the binary — parses args from the process environment.
pub async fn start(args: impl IntoIterator<Item = String>) {
    let args = Args::parse_from(args);
    if let Err(e) = run_with(args, None, None).await {
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
    run_with(args, Some(bind_tx), None).await
}

/// Like [`run`], but also stoppable via `shutdown_rx` — tests use this to
/// exercise graceful shutdown (audit drain) without process signals.
pub async fn run_until(
    args: impl IntoIterator<Item = String>,
    bind_tx: oneshot::Sender<SocketAddr>,
    shutdown_rx: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let args = Args::parse_from(args);
    run_with(args, Some(bind_tx), Some(shutdown_rx)).await
}

async fn run_with(
    args: Args,
    bind_tx: Option<oneshot::Sender<SocketAddr>>,
    shutdown_rx: Option<oneshot::Receiver<()>>,
) -> anyhow::Result<()> {
    init_tracing(&args.log, args.json_logs);

    tracing::info!(?args, "starting byos");

    // Fail-fast: no audit database, no service (ADR-0001 — the audit trail
    // is required by the slashing policy, so "up but not auditing" must be
    // an impossible state).
    let pool = audit::connect_and_migrate(&args.database_url.0).await?;
    let last_id = audit::max_proposal_id(&pool).await?;

    let domain = byos_common::eip712::byos_domain(args.chain_id, args.trampoline_factory);
    let (audit_tx, audit_rx) = tokio::sync::mpsc::unbounded_channel();
    let writer = audit::spawn(pool, audit_rx);
    let store = Arc::new(InMemoryProposalStore::new(audit_tx));
    store.seed_next_id(last_id);

    let default_gas_price = args.default_gas_price.unwrap_or(0);
    let gas_price = Arc::new(AtomicU64::new(default_gas_price));
    let state = AppState::new(store.clone(), domain, gas_price.clone());

    let period = std::time::Duration::from_secs(args.validation_interval_secs);

    // Background validator (ADR-0001, async ingestion). When --rpc-url is
    // set, the composite ProposalValidator gates proposals via on-chain escrow
    // balance checks and settlement simulation. Without an RPC endpoint the
    // service falls back to AcceptAll (useful for tests).
    // clap's `requires_all` on --rpc-url guarantees that --escrow-address,
    // --min-collateral, --default-gas-price, and --settlement-address are
    // present when --rpc-url is set — the unwraps below cannot fail.
    let validation_loop = if let Some(rpc_url) = args.rpc_url {
        let escrow_address = args.escrow_address.unwrap();
        let min_collateral = args.min_collateral.unwrap();
        let settlement_address = args.settlement_address.unwrap();

        let url: reqwest::Url = rpc_url.0.parse().context("invalid --rpc-url")?;
        let provider = alloy::providers::ProviderBuilder::new().connect_http(url);

        // Fail-fast: verify the RPC endpoint is reachable before accepting
        // any proposals that would need escrow checks.
        provider
            .get_block_number()
            .await
            .context("RPC unreachable at startup (--rpc-url)")?;

        let escrow = EscrowValidator::new(
            provider.clone(),
            escrow_address,
            U256::from(min_collateral),
            gas_price,
        );
        let simulation =
            SimulationValidator::new(provider, settlement_address, args.trampoline_factory);
        let validator = ProposalValidator::new(escrow, simulation);
        crate::infra::validation::spawn(store, validator, period)
    } else {
        tracing::warn!("no --rpc-url provided, validation disabled (AcceptAll)");
        crate::infra::validation::spawn(store, crate::domain::validator::AcceptAll, period)
    };

    api::serve(args.public_addr, state, bind_tx, shutdown_rx)
        .await
        .context("public API server exited with error")?;

    // The validation loop holds the store — and with it an audit sender — so
    // stop it first, or the writer's channel never closes and the drain below
    // hangs. A verdict lost mid-tick to the abort is moot: the in-memory
    // store vanishes at shutdown anyway. Then awaiting the writer flushes
    // everything still queued.
    validation_loop.abort();
    writer.await.context("audit writer task panicked")
}

// try_init: a second in-process instance (tests restart the service) must
// not panic on the already-set global subscriber.
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
