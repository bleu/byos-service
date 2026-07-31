//! Deploys the BYOS Escrow onto a chain someone else is running, and prints
//! the addresses the rest of the stack needs.
//!
//! Tier-1 e2e gets its Escrow from [`e2e::chain::Chain::spawn`], which owns its
//! anvil. The offline-mode demo cannot: anvil lives in a container
//! the compose stack started, and the deploy has to happen from outside. This
//! wraps the same [`e2e::chain::deploy_escrow`] helper in a CLI so `just
//! stack-up` can call it.
//!
//! Output is `key=value` lines so a shell can `eval` them:
//!
//! ```sh
//! cargo run -p e2e --example deploy-escrow -- --rpc-url http://127.0.0.1:8545 ...
//! escrow=0x…
//! trampoline_factory=0x…
//! ```
//!
//! Re-running is safe — the deploy is CREATE2 with a fixed salt, so a second
//! run reports the same addresses without sending a transaction.
//!
//! An example rather than a bin target, for the reason `subsolver`'s `propose`
//! gives: `cargo clippy --all-targets` compiles examples, so it cannot rot
//! silently, and it ships in nothing.

use {
    alloy::{
        network::EthereumWallet,
        primitives::Address,
        providers::{Provider, ProviderBuilder},
        signers::local::PrivateKeySigner,
    },
    anyhow::Context,
    clap::Parser,
    e2e::chain::{EscrowRoles, deploy_escrow},
};

#[derive(Parser)]
#[command(about = "Deploy the BYOS Escrow to a running chain via CREATE2")]
struct Args {
    /// JSON-RPC endpoint of the chain to deploy to.
    #[arg(long, env, hide_env_values = true)]
    rpc_url: reqwest::Url,

    /// Escrow admin (`DEFAULT_ADMIN_ROLE`).
    #[arg(long, env)]
    admin: Address,

    /// Escrow operator (`OPERATOR_ROLE`), which signs Track A debits.
    #[arg(long, env)]
    operator: Address,

    /// Account granted `SUBMITTER_ROLE` — the one BYOS settles from. Repeat
    /// the flag for more than one.
    #[arg(long = "submitter", env, required = true, num_args = 1..)]
    submitters: Vec<Address>,

    /// Any funded account; the Escrow's address does not depend on it, since
    /// CREATE2 derives from the factory, the salt and the init code. Env-only
    /// by convention (ADR-0006): CLI arguments are visible to other users via
    /// `ps`.
    #[arg(long, env = "DEPLOYER_PRIVATE_KEY", hide_env_values = true)]
    deployer_key: PrivateKeySigner,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(args.deployer_key))
        .connect_http(args.rpc_url)
        .erased();

    let roles = EscrowRoles {
        admin: args.admin,
        operator: args.operator,
        submitters: args.submitters,
    };
    let escrow = deploy_escrow(&provider, &roles).await?;

    // Read the factory back rather than deriving it: its address depends on the
    // Escrow's own address and deploy nonce, and the Escrow is the contract
    // that knows.
    let trampoline_factory = byos_common::contracts::Escrow::new(escrow, &provider)
        .TRAMPOLINE_FACTORY()
        .call()
        .await
        .context("reading TRAMPOLINE_FACTORY from the deployed Escrow")?;

    println!("escrow={escrow}");
    println!("trampoline_factory={trampoline_factory}");
    Ok(())
}
