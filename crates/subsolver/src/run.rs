//! The continuous polling loop (ADR-0001's sub-solver lifecycle): discover
//! solvable orders, route, sign, submit, and naturally resubmit once a
//! proposal expires — BYOS drops proposals on expiry and simulation failure
//! and never patches or retries them for us.

use {
    crate::{
        domain::{
            proposal::{Order, RouteParams, build_proposal},
            routing,
            signing::proposal_domain,
        },
        infra::{blockchain::ChainClient, byos::ByosClient, orderbook::OrderbookClient},
    },
    alloy::{
        primitives::{Address, B256, Bytes, U256},
        providers::DynProvider,
        signers::local::PrivateKeySigner,
        sol_types::Eip712Domain,
    },
    reqwest::Url,
    std::{collections::HashMap, time::Duration},
};

/// Everything a running sub-solver needs. The e2e crate builds this
/// programmatically; the binary builds it from CLI args + TOML (ADR-0006).
pub struct Config {
    pub orderbook_url: Url,
    pub byos_url: Url,
    pub signer: PrivateKeySigner,
    pub chain_id: u64,
    /// TrampolineFactory: the EIP-712 `verifyingContract` and the oracle for
    /// this sub-solver's Trampoline address.
    pub trampoline_factory: Address,
    pub uniswap_router: Address,
    pub uniswap_factory: Address,
    pub pair_init_code_hash: B256,
    /// How long each submitted proposal stays valid (`valid_until = now +
    /// ttl`).
    pub proposal_ttl: Duration,
    /// Delay between polls when running the continuous loop.
    pub poll_interval: Duration,
    /// Appended to every route and covered by the signature — the injection
    /// point for settlement-time misbehavior in tests.
    pub extra_interactions: Vec<proposal_dto::Interaction>,
}

/// The reference sub-solver: one signer, one BYOS instance, one orderbook.
pub struct Subsolver {
    config: Config,
    orderbook: OrderbookClient,
    byos: ByosClient,
    chain: ChainClient,
    domain: Eip712Domain,
    trampoline: Address,
    /// Live submissions by order UID: no resubmission until they expire.
    live_until: HashMap<Bytes, u64>,
    /// Monotonic salt distinguishing otherwise identical proposals.
    next_nonce: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("orderbook: {0}")]
    Orderbook(#[from] crate::infra::orderbook::Error),
    #[error("chain: {0}")]
    Chain(#[from] crate::infra::blockchain::Error),
}

impl Subsolver {
    /// Builds the sub-solver and resolves its Trampoline address from the
    /// factory. Assumes the signer is already onboarded (escrow deposited);
    /// this crate never sends transactions.
    pub async fn new(config: Config, provider: DynProvider) -> Result<Self, Error> {
        let chain = ChainClient::new(provider);
        let trampoline = chain
            .trampoline(config.trampoline_factory, config.signer.address())
            .await?;
        Ok(Self {
            orderbook: OrderbookClient::new(config.orderbook_url.clone()),
            byos: ByosClient::new(config.byos_url.clone()),
            chain,
            domain: proposal_domain(config.chain_id, config.trampoline_factory),
            trampoline,
            live_until: HashMap::new(),
            next_nonce: 0,
            config,
        })
    }

    /// One pass of the loop: fetch the auction and submit a proposal for
    /// every eligible order without a live one. Per-order failures (no
    /// route, rejection) are logged and skipped — the next poll retries;
    /// only failures that void the whole pass surface as errors.
    pub async fn poll_once(&mut self, now: u64) -> Result<(), Error> {
        self.live_until.retain(|_, valid_until| *valid_until > now);

        for order in self.orderbook.solvable_orders().await? {
            if self.live_until.contains_key(&order.uid) {
                continue;
            }
            match self.propose(&order, now).await {
                Ok(Some(valid_until)) => {
                    self.live_until.insert(order.uid, valid_until);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(order_uid = %order.uid, %error, "skipping order this poll");
                }
            }
        }
        Ok(())
    }

    /// Routes, signs, and submits one order. `Ok(Some(valid_until))` means a
    /// proposal is now live; `Ok(None)` means the order is unroutable at the
    /// current reserves (no pool, can't beat the limit price).
    async fn propose(&mut self, order: &Order, now: u64) -> Result<Option<u64>, ProposeError> {
        let pair = routing::pair_address(
            self.config.uniswap_factory,
            self.config.pair_init_code_hash,
            order.sell_token,
            order.buy_token,
        );
        let (reserve_sell, reserve_buy) = self
            .chain
            .reserves(pair, order.sell_token, order.buy_token)
            .await?;

        let valid_until = now + self.config.proposal_ttl.as_secs();
        let params = RouteParams {
            router: self.config.uniswap_router,
            trampoline: self.trampoline,
            reserve_sell,
            reserve_buy,
            valid_until,
            nonce: U256::from(self.next_nonce),
            extra_interactions: self.config.extra_interactions.clone(),
        };
        let Some(proposal) = build_proposal(order, &params, &self.domain, &self.config.signer)
        else {
            return Ok(None);
        };
        self.next_nonce += 1;

        let id = self.byos.submit(&proposal).await?;
        tracing::info!(order_uid = %order.uid, id, valid_until, "proposal submitted");
        Ok(Some(valid_until))
    }
}

#[derive(Debug, thiserror::Error)]
enum ProposeError {
    #[error("chain: {0}")]
    Chain(#[from] crate::infra::blockchain::Error),
    #[error("byos: {0}")]
    Byos(#[from] crate::infra::byos::Error),
}

/// Binary entry point: parse CLI + TOML, build the sub-solver, and poll
/// until SIGINT. `main.rs` stays thin per ADR-0005.
pub async fn start(args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    use {alloy::providers::Provider, clap::Parser};

    let args = crate::infra::cli::Args::parse_from(args);
    tracing_subscriber::fmt()
        .with_env_filter(args.log.clone())
        .init();
    tracing::info!(?args, version = env!("CARGO_PKG_VERSION"), "starting");

    let file = crate::infra::config::Config::from_toml(&std::fs::read_to_string(&args.config)?)?;
    let extra_interactions = if file.append_revert {
        // An unknown selector on the router: no fallback, so the call — and
        // with it every settlement carrying this route — reverts.
        vec![proposal_dto::Interaction {
            target: file.uniswap_router,
            value: U256::ZERO,
            call_data: vec![0xba, 0x5e, 0xba, 0x11].into(),
        }]
    } else {
        vec![]
    };
    let config = Config {
        orderbook_url: args.orderbook_url,
        byos_url: args.byos_url,
        signer: args.private_key,
        chain_id: file.chain_id,
        trampoline_factory: file.trampoline_factory,
        uniswap_router: file.uniswap_router,
        uniswap_factory: file.uniswap_factory,
        pair_init_code_hash: file.pair_init_code_hash,
        proposal_ttl: file.proposal_ttl,
        poll_interval: file.poll_interval,
        extra_interactions,
    };

    let provider = alloy::providers::ProviderBuilder::new()
        .connect_http(args.rpc_url)
        .erased();
    let mut subsolver = Subsolver::new(config, provider).await?;

    let mut interval = tokio::time::interval(subsolver.config.poll_interval);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock after the unix epoch")
                    .as_secs();
                if let Err(error) = subsolver.poll_once(now).await {
                    tracing::warn!(%error, "poll failed; retrying next interval");
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::domain::signing::{UnsignedProposal, proposal_domain},
        alloy::{
            primitives::{Address, B256, Bytes, Signature, U256, address, b256},
            providers::{Provider, ProviderBuilder},
            transports::mock::Asserter,
        },
        serde_json::json,
        std::time::Duration,
        wiremock::{
            Mock,
            MockServer,
            ResponseTemplate,
            matchers::{method, path},
        },
    };

    const WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const USDC: Address = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    const FACTORY: Address = address!("0x00000000000000000000000000000000000fac70");
    const TRAMPOLINE: Address = address!("0x00000000000000000000000000000000f00dbabe");

    fn config(orderbook: &MockServer, byos: &MockServer) -> Config {
        Config {
            orderbook_url: orderbook.uri().parse().unwrap(),
            byos_url: byos.uri().parse().unwrap(),
            signer: alloy::signers::local::PrivateKeySigner::from_bytes(
                &U256::from(0xA11CE).into(),
            )
            .unwrap(),
            chain_id: 31337,
            trampoline_factory: FACTORY,
            uniswap_router: address!("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D"),
            uniswap_factory: address!("0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f"),
            pair_init_code_hash: b256!(
                "0x96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f"
            ),
            proposal_ttl: Duration::from_secs(60),
            poll_interval: Duration::from_secs(2),
            extra_interactions: vec![],
        }
    }

    fn reserves_return(reserve0: u64, reserve1: u64) -> Bytes {
        let words = [
            U256::from(reserve0),
            U256::from(reserve1),
            U256::from(1_750_000_000u64),
        ];
        words
            .iter()
            .flat_map(|word| B256::from(*word).0)
            .collect::<Vec<u8>>()
            .into()
    }

    async fn mock_auction(orderbook: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/v1/auction"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 1,
                "orders": [{
                    "uid": format!("0x{}", "11".repeat(56)),
                    "sellToken": WETH,
                    "buyToken": USDC,
                    "sellAmount": "1000",
                    "buyAmount": "900",
                    "kind": "sell",
                    "partiallyFillable": false,
                    "appData": B256::ZERO,
                }],
            })))
            .mount(orderbook)
            .await;
    }

    #[tokio::test]
    async fn submits_once_per_proposal_lifetime_and_resubmits_on_expiry() {
        let orderbook = MockServer::start().await;
        let byos = MockServer::start().await;
        mock_auction(&orderbook).await;
        Mock::given(method("POST"))
            .and(path("/proposals"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
            .mount(&byos)
            .await;

        let asserter = Asserter::new();
        asserter.push_success(&Bytes::from(
            B256::left_padding_from(TRAMPOLINE.as_slice()).0,
        ));
        // One getReserves per routed poll: the initial submission and the
        // post-expiry resubmission. The held poll in between must not hit RPC.
        asserter.push_success(&reserves_return(5_000_000, 10_000)); // USDC, WETH
        asserter.push_success(&reserves_return(5_000_000, 10_000));

        let config = config(&orderbook, &byos);
        let sub_solver_address = config.signer.address();
        let provider = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();
        let mut subsolver = Subsolver::new(config, provider).await.unwrap();

        let t0 = 1_750_000_000u64;
        subsolver.poll_once(t0).await.unwrap();
        subsolver.poll_once(t0 + 1).await.unwrap(); // proposal still live: held
        subsolver.poll_once(t0 + 61).await.unwrap(); // expired: resubmitted

        let submissions: Vec<proposal_dto::Proposal> = byos
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect();
        assert_eq!(submissions.len(), 2);

        // First submission: routed against the mocked reserves, valid for
        // the configured ttl, signed by the sub-solver in the factory-
        // anchored domain.
        let first = &submissions[0];
        assert_eq!(first.order_uid, Bytes::from(vec![0x11; 56]));
        assert_eq!(first.sell_amount, U256::from(1000));
        // amount_out(1000, 10_000 WETH, 5_000_000 USDC) = 453_305
        assert_eq!(first.buy_amount, U256::from(453_305));
        assert_eq!(first.valid_until, t0 + 60);

        let domain = proposal_domain(31337, FACTORY);
        let unsigned = UnsignedProposal {
            order_uid: &first.order_uid,
            sell_amount: first.sell_amount,
            buy_amount: first.buy_amount,
            interactions: &first.interactions,
            valid_until: first.valid_until,
            nonce: first.nonce,
        };
        let signature = Signature::from_raw(&first.signature).unwrap();
        let recovered = signature
            .recover_address_from_prehash(&unsigned.signing_digest(&domain))
            .unwrap();
        assert_eq!(recovered, sub_solver_address);

        // The resubmission is a fresh proposal: new lifetime, new nonce.
        let second = &submissions[1];
        assert_eq!(second.valid_until, t0 + 61 + 60);
        assert_ne!(second.nonce, first.nonce);
    }

    #[tokio::test]
    async fn typed_rejections_do_not_kill_the_loop() {
        let orderbook = MockServer::start().await;
        let byos = MockServer::start().await;
        mock_auction(&orderbook).await;
        Mock::given(method("POST"))
            .and(path("/proposals"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "kind": "UnderCollateralized",
                "description": "escrow below gas + c_l",
            })))
            .mount(&byos)
            .await;

        let asserter = Asserter::new();
        asserter.push_success(&Bytes::from(
            B256::left_padding_from(TRAMPOLINE.as_slice()).0,
        ));
        asserter.push_success(&reserves_return(5_000_000, 10_000));
        asserter.push_success(&reserves_return(5_000_000, 10_000));

        let provider = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();
        let mut subsolver = Subsolver::new(config(&orderbook, &byos), provider)
            .await
            .unwrap();

        // A rejected proposal is not recorded as live: the next poll tries
        // again (sub-solvers naturally resubmit, ADR-0001).
        subsolver.poll_once(1_750_000_000).await.unwrap();
        subsolver.poll_once(1_750_000_001).await.unwrap();
        assert_eq!(byos.received_requests().await.unwrap().len(), 2);
    }
}
