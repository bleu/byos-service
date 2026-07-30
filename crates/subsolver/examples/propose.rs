//! Submits one signed proposal to a BYOS instance and watches the background
//! validator pick it up. Run it against `just byos-local` (see the README):
//!
//! ```sh
//! just byos-local   # one shell
//! just propose      # another
//! ```
//!
//! This is a tool, not a test — it prints what it observes and never asserts.
//! The service-level tests (`just test-db`) do the asserting.
//!
//! It needs no chain. `POST /proposals` recovers an EIP-712 signer and
//! `GET /proposal/{id}` wants a signed `ReadAuth` bearer, so curl alone
//! cannot drive the API; everything else here is pure computation. That is
//! also why this skips [`subsolver::Subsolver`], which resolves its
//! Trampoline through an eth_call before its loop even starts.
//!
//! An example rather than a second bin target: `cargo clippy --all-targets`
//! compiles examples, so it cannot rot silently, and it ships in nothing.

use {
    alloy::{
        primitives::{Address, U256},
        signers::local::PrivateKeySigner,
    },
    byos_common::{contracts::Interaction, eip712},
    clap::Parser,
    proposal_dto::{error::Kind, proposal::Status},
    std::time::{Duration, SystemTime, UNIX_EPOCH},
    subsolver::{domain::proposal::SignedProposal, infra::byos},
};

/// How long to wait for the validator to move the proposal off `submitted`.
/// Longer than byos's own 12s `--validation-interval-secs` default: `just
/// byos-local` passes 2s, but run against a service someone started by hand
/// and a shorter timeout reports a dead validation loop when the loop has
/// simply not ticked yet — the exact false alarm this tool exists to rule out.
const POLL_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Proposal lifetime. Short, because the whole run is over in seconds — and
/// the fixtures' `1750000000` is June 2025, which ingestion rejects outright.
const PROPOSAL_TTL: u64 = 60;

#[derive(Parser)]
#[command(about = "Submit one signed proposal to a local BYOS and follow its status")]
struct Args {
    /// Base URL of the BYOS public listener.
    #[arg(long, env, default_value = "http://127.0.0.1:9585")]
    byos_url: reqwest::Url,

    /// Must match the running service's `--chain-id`, or the signature
    /// recovers a different address and the read 404s.
    #[arg(long, env)]
    chain_id: u64,

    /// Must match the running service's `--trampoline-factory`, for the same
    /// reason as `--chain-id`.
    #[arg(long, env)]
    trampoline_factory: Address,

    /// The sub-solver identity. Env-only by convention (ADR-0006): CLI
    /// arguments are visible to other users via `ps`.
    #[arg(long, env = "SUBSOLVER_PRIVATE_KEY", hide_env_values = true)]
    private_key: PrivateKeySigner,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let signer = args.private_key;
    let domain = eip712::byos_domain(args.chain_id, args.trampoline_factory);
    let client = byos::ByosClient::new(args.byos_url.clone(), domain.clone(), signer.clone());

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let proposal = sign(&signer, &domain, now).await;

    println!("sub-solver  {}", signer.address());
    println!("byos        {}", args.byos_url);
    println!(
        "domain      chain {} / {}",
        args.chain_id, args.trampoline_factory
    );
    println!(
        "validUntil  {} (now + {PROPOSAL_TTL}s)",
        proposal.valid_until
    );

    let id = client.submit(&proposal).await?;
    println!("\n202 accepted, id {id} — the signature verified, nothing more");

    follow(&client, id).await
}

/// A minimal proposal: the route is never executed here, since a service
/// without `--rpc-url` validates with AcceptAll. Fixed order UID so
/// `GET /proposals/{order_uid}` is worth trying by hand; clock-seeded nonce
/// so re-runs stay distinguishable.
async fn sign(
    signer: &PrivateKeySigner,
    domain: &alloy::sol_types::Eip712Domain,
    now: u64,
) -> SignedProposal {
    let mut proposal = SignedProposal {
        order_uid: vec![0xab; 56].into(),
        sell_amount: U256::from(1_000_000u64),
        buy_amount: U256::from(990_000u64),
        interactions: vec![Interaction {
            target: Address::repeat_byte(0xdd),
            value: U256::ZERO,
            callData: vec![0xde, 0xad].into(),
        }],
        valid_until: now + PROPOSAL_TTL,
        nonce: U256::from(now),
        signature: Default::default(),
    };
    let signature =
        eip712::sign_proposal(signer, domain, &proposal.onchain(), &proposal.interactions)
            .await
            .expect("in-memory ECDSA signing is infallible");
    proposal.signature = signature.as_bytes().into();
    proposal
}

/// Polls the verdict, printing every status change. A single GET would only
/// ever show `submitted`, which says nothing about whether the background
/// loop is alive — the `submitted` -> `active` transition is the point.
async fn follow(client: &byos::ByosClient, id: u64) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + POLL_TIMEOUT;
    let mut last = None;

    loop {
        match client.proposal(id).await {
            Ok(view) => {
                if last != Some(view.status) {
                    println!("status      {:?}", view.status);
                    last = Some(view.status);
                }
                // Anything past `submitted` means the validator ran. In
                // AcceptAll that is `active`, which proves the loop is alive
                // — not that anything was checked.
                if view.status != Status::Submitted {
                    return Ok(());
                }
            }
            Err(byos::Error::Rejected(error)) if error.kind == Kind::ProposalNotFound => {
                anyhow::bail!(
                    "the service accepted this proposal and then 404'd it: --chain-id or \
                     --trampoline-factory disagrees with the running service, so the read \
                     signature recovers a different sub-solver (reads are owner-scoped, ADR-0011)"
                );
            }
            Err(error) => return Err(error.into()),
        }

        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "still `submitted` after {POLL_TIMEOUT:?} — the validation loop is not running \
                 (is the service up? did it boot with --validation-interval-secs?)"
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
