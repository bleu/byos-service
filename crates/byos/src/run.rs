//! Startup: parse CLI args → init tracing → build app → serve with graceful
//! shutdown. The `run()` variant accepts a `oneshot::Sender<BoundAddrs>` so
//! e2e tests can discover the bound ports.

use {
    crate::infra::{
        api::{self, AppState},
        audit,
        blockchain::{
            escrow::EscrowValidator,
            validator::{ProposalValidator, SimulationValidator},
        },
        storage::ProposalStore,
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

    /// Public API listener address (proposals endpoints). Loopback by
    /// default so a dev instance does not land on the LAN; deployments set
    /// the bind address explicitly anyway.
    #[arg(long, env, default_value = "127.0.0.1:9585")]
    public_addr: SocketAddr,

    /// Internal API listener address (`/solve`). Only our co-deployed driver
    /// may call `/solve` — the proposal book it returns is MEV-relevant — so
    /// keep this address unreachable from the internet. Defaults
    /// to loopback.
    #[arg(long, env, default_value = "127.0.0.1:9586")]
    internal_addr: SocketAddr,

    /// Optional shared secret for `/solve`: when set, requests must carry
    /// `Authorization: Bearer <token>`. The driver sends it via its
    /// `[solver.request-headers]` config. Defense-in-depth on top of the
    /// listener split, not a substitute for it. Prefer the SOLVE_BEARER_TOKEN
    /// env var — CLI arguments are visible to other users via `ps`.
    #[arg(long, env)]
    solve_bearer_token: Option<SolveBearerToken>,

    /// Chain ID for the EIP-712 domain.
    #[arg(long, env)]
    chain_id: u64,

    /// TrampolineFactory contract address (EIP-712 `verifyingContract`).
    #[arg(long, env)]
    trampoline_factory: alloy::primitives::Address,

    /// Postgres URL for the proposal store and the audit trail (ADR-0013,
    /// ADR-0001 write-behind). Required:
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
    #[arg(long, env, requires_all = ["escrow_address", "min_collateral", "default_gas_price", "settlement_address", "orderbook_url"])]
    rpc_url: Option<RpcUrl>,

    /// CoW orderbook base URL (e.g. https://api.cow.fi/mainnet) for fetching
    /// the orders proposals settle. Required when `--rpc-url` is set.
    #[arg(long, env)]
    orderbook_url: Option<reqwest::Url>,

    /// Escrow contract address for sub-solver balance checks. Required when
    /// `--rpc-url` is set.
    #[arg(long, env)]
    escrow_address: Option<alloy::primitives::Address>,

    /// GPv2Settlement contract address. The simulation's `settle()` target.
    /// Required when `--rpc-url` is set.
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

    /// How long dropped proposals (rejected/simFailed/expired/cancelled)
    /// stay readable after reaching their terminal state; the retention
    /// sweep deletes them past this window and they 404. Their audit trail
    /// is kept regardless. Accepts humantime strings, e.g. `1h`, `30m`.
    #[arg(long, env, default_value = "1h", value_parser = humantime::parse_duration)]
    dropped_retention: std::time::Duration,

    /// Seconds between retention sweep passes. Deliberately slow — dropped
    /// proposals only need to disappear on the order of the retention
    /// window, not of a block.
    #[arg(long, env, default_value_t = 300)]
    retention_sweep_interval_secs: u64,

    /// Maximum proposal lifetime in seconds (ADR-0013): `POST /proposals`
    /// rejects any `validUntil` further out than this. Bounds the worst-case
    /// simulation cost per proposal.
    #[arg(long, env, default_value_t = 300)]
    max_proposal_lifetime_secs: u64,

    /// How long a proposal may sit in `Executing` before falling back to
    /// `Active` (ADR-0013's lost-notification backstop). Re-simulation
    /// reconciles reality if the settlement actually landed. Accepts
    /// humantime strings, e.g. `5m`.
    #[arg(long, env, default_value = "5m", value_parser = humantime::parse_duration)]
    executing_timeout: std::time::Duration,

    /// Profitability floor in wei (ADR-0013): the first simulation rejects
    /// proposals whose score (`surplus + fee - gas`, ADR-0002) does not
    /// exceed this. The default 0 mirrors /solve's own score > 0 rule.
    #[arg(long, env, default_value_t = 0)]
    min_proposal_score: u128,

    /// Private key of the escrow operator account (`OPERATOR_ROLE` on the
    /// Escrow contract), enabling the Track A penalty loop (ADR-0003).
    /// When omitted, debits are disabled and `SettleFailed`
    /// proposals wait. Prefer the OPERATOR_PRIVATE_KEY env var — CLI
    /// arguments are visible to other users via `ps`.
    #[arg(long, env, requires = "rpc_url")]
    operator_private_key: Option<OperatorPrivateKey>,
}

/// Connection-string wrapper whose `Debug` hides the value, so the startup
/// `?args` log can't leak the password (ADR-0006: secrets redact themselves).
#[derive(Clone)]
struct DatabaseUrl(String);

/// RPC URL wrapper whose `Debug` hides the value — the URL may contain
/// API keys (ADR-0006: secrets redact themselves).
#[derive(Clone)]
struct RpcUrl(String);

/// Bearer-token wrapper whose `Debug` hides the value (ADR-0006: secrets
/// redact themselves).
#[derive(Clone)]
struct SolveBearerToken(String);

/// Operator-key wrapper whose `Debug` hides the value (ADR-0006: secrets
/// redact themselves). Parses at arg time so a malformed key fails startup,
/// not the first debit.
#[derive(Clone)]
struct OperatorPrivateKey(alloy::signers::local::PrivateKeySigner);

impl std::str::FromStr for OperatorPrivateKey {
    type Err = alloy::signers::local::LocalSignerError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

impl std::fmt::Debug for OperatorPrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl std::str::FromStr for SolveBearerToken {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_owned()))
    }
}

impl std::fmt::Debug for SolveBearerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

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

/// Entry point for tests — caller passes args and receives the bound
/// addresses.
pub async fn run(
    args: impl IntoIterator<Item = String>,
    bind_tx: oneshot::Sender<api::BoundAddrs>,
) -> anyhow::Result<()> {
    let args = Args::parse_from(args);
    run_with(args, Some(bind_tx), None).await
}

/// Like [`run`], but also stoppable via `shutdown_rx` — tests use this to
/// exercise graceful shutdown (audit drain) without process signals.
pub async fn run_until(
    args: impl IntoIterator<Item = String>,
    bind_tx: oneshot::Sender<api::BoundAddrs>,
    shutdown_rx: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let args = Args::parse_from(args);
    run_with(args, Some(bind_tx), Some(shutdown_rx)).await
}

async fn run_with(
    args: Args,
    bind_tx: Option<oneshot::Sender<api::BoundAddrs>>,
    shutdown_rx: Option<oneshot::Receiver<()>>,
) -> anyhow::Result<()> {
    init_tracing(&args.log, args.json_logs);

    tracing::info!(?args, "starting byos");

    // Fail-fast: no database, no service (ADR-0001/ADR-0013 — Postgres holds
    // both the proposal state and the audit trail the slashing policy
    // requires, so "up but not persisting" must be an impossible state).
    let pool = audit::connect_and_migrate(&args.database_url.0).await?;

    let domain = byos_common::eip712::byos_domain(args.chain_id, args.trampoline_factory);
    let (audit_tx, audit_rx) = tokio::sync::mpsc::unbounded_channel();
    let writer = audit::spawn(pool.clone(), audit_rx);
    let store = Arc::new(ProposalStore::new(pool, audit_tx));

    let default_gas_price = args.default_gas_price.unwrap_or(0);
    let gas_price = Arc::new(AtomicU64::new(default_gas_price));
    let state = AppState::new(
        store.clone(),
        domain,
        gas_price.clone(),
        args.max_proposal_lifetime_secs,
    );

    let period = std::time::Duration::from_secs(args.validation_interval_secs);

    // Nothing below spawns a background loop until every fallible setup step
    // has passed: a task spawned before an early `?` return would outlive
    // the failure with no one left to abort it.
    let mut penalty_loop = None;

    // Background validator (ADR-0001, async ingestion). When --rpc-url is
    // set, the composite ProposalValidator gates proposals via on-chain escrow
    // balance checks and settlement simulation. Without an RPC endpoint the
    // service falls back to AcceptAll (useful for tests).
    // clap's `requires_all` on --rpc-url guarantees that --escrow-address,
    // --min-collateral, --default-gas-price, and --settlement-address are
    // present when --rpc-url is set — the unwraps below cannot fail.
    let validation_loop = if let Some(rpc_url) = args.rpc_url {
        let escrow_address = args
            .escrow_address
            .expect("clap requires_all guarantees it");
        let min_collateral = args
            .min_collateral
            .expect("clap requires_all guarantees it");
        let settlement_address = args
            .settlement_address
            .expect("clap requires_all guarantees it");

        let url: reqwest::Url = rpc_url.0.parse().context("invalid --rpc-url")?;
        let provider = alloy::providers::ProviderBuilder::new().connect_http(url.clone());

        // Fail-fast: verify the RPC endpoint is reachable before accepting
        // any proposals that would need escrow checks. Runs before the
        // spawns below so an unreachable node leaves no orphaned tasks.
        provider
            .get_block_number()
            .await
            .context("RPC unreachable at startup (--rpc-url)")?;

        // Track A penalty loop (ADR-0003): debits SettleFailed
        // reverts and queued non-settlement charges from escrow. Without the
        // operator key the service still observes outcomes; the debits just
        // wait for an operator-enabled instance.
        penalty_loop = if let Some(operator_key) = args.operator_private_key {
            let operator_provider = alloy::providers::ProviderBuilder::new()
                .wallet(operator_key.0)
                .connect_http(url);
            let operator = crate::infra::blockchain::operator::EscrowOperator::new(
                operator_provider,
                escrow_address,
            );
            Some(crate::infra::penalty::spawn(
                store.clone(),
                operator,
                period,
                U256::from(min_collateral),
            ))
        } else {
            tracing::warn!("no --operator-private-key provided, Track A debits disabled");
            None
        };

        let escrow = EscrowValidator::new(
            provider.clone(),
            escrow_address,
            U256::from(min_collateral),
            gas_price.clone(),
        );
        let orderbook = crate::infra::orderbook::OrderbookClient::new(
            args.orderbook_url.expect("clap requires_all guarantees it"),
        );
        let simulation = SimulationValidator::new(
            provider,
            orderbook,
            settlement_address,
            escrow_address,
            args.trampoline_factory,
            gas_price,
            U256::from(args.min_proposal_score),
        );
        let validator = ProposalValidator::new(escrow, simulation);
        crate::infra::validation::spawn(store.clone(), validator, period, args.executing_timeout)
    } else {
        tracing::warn!("no --rpc-url provided, validation disabled (AcceptAll)");
        crate::infra::validation::spawn(
            store.clone(),
            crate::domain::validator::AcceptAll,
            period,
            args.executing_timeout,
        )
    };

    // Retention sweep (ADR-0013): bounds the proposals table by deleting
    // dropped-tier rows past their window. audit_events is never touched.
    // Takes the last `store` handle by value: every store clone holds an
    // audit sender, so one left alive in this scope would keep the channel
    // open and hang the `writer.await` drain below.
    let retention_loop = crate::infra::retention::spawn(
        store,
        std::time::Duration::from_secs(args.retention_sweep_interval_secs),
        args.dropped_retention,
    );

    // Bound, not `?`: teardown has to run even when serving fails, or a bind
    // error leaves three loops polling the database with nobody to stop them
    // and the queued audit events unflushed. In-process callers (`run`,
    // `run_until`) are where that leak is observable.
    let served = api::serve(
        args.public_addr,
        args.internal_addr,
        state,
        args.solve_bearer_token.as_ref().map(|t| t.0.as_str()),
        bind_tx,
        shutdown_rx,
    )
    .await;

    // The validation, retention, and penalty loops hold the store — and
    // with it an audit sender — so stop them first, or the writer's channel
    // never closes and the drain below hangs. A verdict or debit lost
    // mid-tick to the abort is redone by the first tick after the next boot
    // — proposals and pending penalties are durable. Then awaiting the
    // writer flushes everything still queued.
    validation_loop.abort();
    retention_loop.abort();
    if let Some(penalty_loop) = penalty_loop {
        penalty_loop.abort();
    }
    // Log the serve error before draining, not after. The writer retries every
    // insert with backoff and never gives up, so with Postgres unreachable and
    // events queued the drain does not return — and the startup diagnostic an
    // operator actually needs would be held hostage behind it. (That hang
    // already existed on the graceful-shutdown path; this keeps it from
    // swallowing the reason we are shutting down.)
    if let Err(e) = &served {
        tracing::error!(%e, "API server exited with error; draining audit trail");
    }
    // Evidence first: whatever went wrong with the listeners, the audit trail
    // up to that point is worth keeping. The serve error is the actionable
    // diagnostic, so it wins the return value; a writer panic has already gone
    // through tracing by the time we get here.
    let drained = writer.await.context("audit writer task panicked");
    served.context("API server exited with error").and(drained)
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
