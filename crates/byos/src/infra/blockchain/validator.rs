//! Composite proposal validator and simulation validator.
//!
//! [`SimulationValidator`] fetches the proposal's order from the orderbook,
//! checks the validation envelope, and dispatches `eth_estimateGas` for a
//! full one-trade `settle()` under the two ADR-0012 state overrides.
//!
//! [`ProposalValidator`] composes
//! [`EscrowValidator`](super::escrow::EscrowValidator)
//! and [`SimulationValidator`] in sequence: escrow first (cheap cached read),
//! then simulation (expensive RPC call).

use {
    super::{escrow::EscrowValidator, simulation},
    crate::{
        domain::{
            proposal::{Proposal, ProposalStatus},
            scoring,
            validator::{RejectionReason, SimulationOutcome, ValidateProposal, Verdict},
        },
        infra::orderbook::{FetchOrder, OrderbookError},
    },
    alloy::{
        primitives::{Address, U256},
        providers::Provider,
        transports::RpcError,
    },
    byos_common::{contracts::TrampolineFactory, settlement::OrderKind},
    parking_lot::Mutex,
    std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    },
};

// The primitives GPv2Settlement binding has no `authenticator()`; a minimal
// local binding fills the gap.
alloy::sol! {
    #[sol(rpc)]
    interface ISettlementAuthenticator {
        function authenticator() external view returns (address);
    }
}

// ---------------------------------------------------------------------------
// SimulationValidator
// ---------------------------------------------------------------------------

/// Validates proposals by simulating them as a full `settle()` via
/// `eth_estimateGas` (ADR-0012). Resolves and caches the per-sub-solver
/// trampoline address (`TrampolineFactory.addressOf`) and the settlement's
/// authenticator address; both are immutable on-chain.
pub struct SimulationValidator<P, O> {
    provider: P,
    orderbook: O,
    settlement_address: Address,
    escrow_address: Address,
    trampoline_factory: Address,
    /// Last-seen auction gas price, shared with `/solve` — the "current gas
    /// price" of the profitability gate (ADR-0013).
    gas_price: Arc<AtomicU64>,
    /// Profitability floor in wei: the first simulation rejects proposals
    /// whose score does not exceed this (`--min-proposal-score`, default 0).
    min_score: U256,
    /// Cached trampoline addresses: sub_solver → trampoline. Persistent across
    /// ticks (trampoline addresses are deterministic and never change).
    trampoline_cache: Mutex<HashMap<Address, Address>>,
    /// Cached `settlement.authenticator()`, resolved on first use.
    authenticator: Mutex<Option<Address>>,
}

impl<P: Provider, O: FetchOrder> SimulationValidator<P, O> {
    pub fn new(
        provider: P,
        orderbook: O,
        settlement_address: Address,
        escrow_address: Address,
        trampoline_factory: Address,
        gas_price: Arc<AtomicU64>,
        min_score: U256,
    ) -> Self {
        Self {
            provider,
            orderbook,
            settlement_address,
            escrow_address,
            trampoline_factory,
            gas_price,
            min_score,
            trampoline_cache: Mutex::new(HashMap::new()),
            authenticator: Mutex::new(None),
        }
    }

    /// Resolve the trampoline address for a sub-solver. Returns from cache if
    /// available; otherwise calls `TrampolineFactory.addressOf` via RPC.
    async fn resolve_trampoline(
        &self,
        sub_solver: Address,
    ) -> Result<Address, alloy::contract::Error> {
        if let Some(&addr) = self.trampoline_cache.lock().get(&sub_solver) {
            return Ok(addr);
        }

        let factory = TrampolineFactory::new(self.trampoline_factory, &self.provider);
        let addr = factory.addressOf(sub_solver).call().await?;

        self.trampoline_cache.lock().insert(sub_solver, addr);
        Ok(addr)
    }

    /// The profitability gate (ADR-0013): scores the proposal against its
    /// order (`score = surplus + fee - gas`, ADR-0002) with the simulated gas
    /// and the last-seen gas price.
    ///
    /// - `Some(Ok(()))` — score exceeds the minimum, proposal may activate.
    /// - `Some(Err(Unprofitable))` — score too low, or the orderbook cannot
    ///   price the surplus token (an auction couldn't either, so the proposal
    ///   could never win `/solve`).
    /// - `None` — transient price-fetch failure, defer to the next tick.
    async fn profitability(
        &self,
        proposal: &Proposal,
        record: &crate::domain::order::OrderRecord,
        gas: u64,
    ) -> Option<Result<(), RejectionReason>> {
        let surplus_token = scoring::surplus_token(
            record.order.kind == OrderKind::Sell,
            record.order.sell_token,
            record.order.buy_token,
        );
        let native_price = match self.orderbook.native_price(surplus_token).await {
            Ok(price) => price,
            Err(OrderbookError::NotFound) => {
                tracing::info!(
                    id = %proposal.id,
                    token = %surplus_token,
                    "orderbook has no native price for the surplus token, rejecting",
                );
                return Some(Err(RejectionReason::Unprofitable));
            }
            Err(OrderbookError::Transient(e)) => {
                tracing::warn!(
                    id = %proposal.id,
                    error = %e,
                    "native price fetch failed (transient), deferring to next tick",
                );
                return None;
            }
        };

        let gas_cost = U256::from(scoring::effective_gas(gas))
            .saturating_mul(U256::from(self.gas_price.load(Ordering::Relaxed)));
        let score = scoring::score_proposal(&scoring::ScoreInput {
            order_sell: record.order.sell_amount,
            order_buy: record.order.buy_amount,
            proposal_sell: proposal.sell_amount,
            proposal_buy: proposal.buy_amount,
            is_sell_order: record.order.kind == OrderKind::Sell,
            gas_cost,
            native_price,
        });
        if score.is_none_or(|s| s <= self.min_score) {
            tracing::info!(
                id = %proposal.id,
                ?score,
                min_score = %self.min_score,
                "proposal scores at or below the minimum, rejecting",
            );
            return Some(Err(RejectionReason::Unprofitable));
        }
        Some(Ok(()))
    }

    /// Resolve `settlement.authenticator()`, from cache after the first call.
    async fn resolve_authenticator(&self) -> Result<Address, alloy::contract::Error> {
        if let Some(addr) = *self.authenticator.lock() {
            return Ok(addr);
        }

        let settlement = ISettlementAuthenticator::new(self.settlement_address, &self.provider);
        let addr = settlement.authenticator().call().await?;

        *self.authenticator.lock() = Some(addr);
        Ok(addr)
    }
}

impl<P: Provider + Send + Sync, O: FetchOrder> ValidateProposal for SimulationValidator<P, O> {
    async fn validate(&self, proposal: &Proposal) -> Option<Verdict> {
        // 1. Fetch the order (cheap after first fetch — forever cache) and check the
        //    envelope before spending any RPC calls.
        let record = match self.orderbook.order(&proposal.order_uid).await {
            Ok(record) => record,
            Err(OrderbookError::NotFound) => {
                tracing::info!(
                    id = %proposal.id,
                    "order unknown to the orderbook, rejecting",
                );
                return Some(Verdict::Reject(RejectionReason::OrderNotFound));
            }
            Err(OrderbookError::Transient(e)) => {
                tracing::warn!(
                    id = %proposal.id,
                    error = %e,
                    "orderbook fetch failed (transient), deferring to next tick",
                );
                return None;
            }
        };
        if let Err(reason) = record.check_envelope(proposal) {
            return Some(Verdict::Reject(reason));
        }

        // 2. Resolve trampoline address. If already stored on the proposal
        //    (re-validation), skip the RPC call; otherwise resolve from the factory (or
        //    its cache).
        let trampoline = match proposal.trampoline {
            Some(addr) => addr,
            None => match self.resolve_trampoline(proposal.sub_solver).await {
                Ok(addr) => addr,
                Err(e) if is_trampoline_revert(&e) => {
                    tracing::info!(
                        id = %proposal.id,
                        sub_solver = %proposal.sub_solver,
                        error = %e,
                        "trampoline resolution reverted, marking SimFailed",
                    );
                    return Some(Verdict::SimFailed);
                }
                Err(e) => {
                    tracing::warn!(
                        id = %proposal.id,
                        sub_solver = %proposal.sub_solver,
                        error = %e,
                        "trampoline resolution failed (transient), deferring to next tick",
                    );
                    return None;
                }
            },
        };

        // 3. Resolve the authenticator (one RPC call ever). Any failure is transient:
        //    authenticator() on GPv2Settlement cannot revert.
        let authenticator = match self.resolve_authenticator().await {
            Ok(addr) => addr,
            Err(e) => {
                tracing::warn!(
                    id = %proposal.id,
                    error = %e,
                    "authenticator resolution failed (transient), deferring to next tick",
                );
                return None;
            }
        };

        // 4. Build the full-settle simulation.
        let on_chain_proposal = byos_common::contracts::Proposal {
            orderUidHash: proposal.order_uid_hash,
            sellAmount: proposal.sell_amount,
            buyAmount: proposal.buy_amount,
            validUntil: proposal.valid_until,
            nonce: proposal.nonce,
        };

        let sim = simulation::build_simulation(&simulation::SimulationParams {
            settlement: self.settlement_address,
            authenticator,
            escrow: self.escrow_address,
            trampoline,
            order: &record.order,
            proposal: on_chain_proposal,
            route: &proposal.interactions,
            signature: &proposal.signature,
        });

        // 5. Dispatch eth_estimateGas under the two state overrides.
        match self
            .provider
            .estimate_gas(sim.tx)
            .account_override(sim.authenticator_override.0, sim.authenticator_override.1)
            .account_override(sim.escrow_override.0, sim.escrow_override.1)
            .await
        {
            Ok(gas) => {
                // The node's answer is stored in a BIGINT column and fed to
                // scoring arithmetic, so a value it cannot hold is a bad
                // answer rather than an expensive proposal — no real
                // settlement approaches a block gas limit. Defer: the next
                // tick asks again, possibly of a healthier node.
                if i64::try_from(gas).is_err() {
                    tracing::warn!(
                        id = %proposal.id, gas,
                        "simulation returned an implausible gas value; deferring"
                    );
                    return None;
                }
                // 6. Profitability gate (ADR-0013), first simulation only: re-validation skips
                //    it so gas-price wobble cannot churn Active proposals; /solve re-scores at
                //    auction time.
                if proposal.status == ProposalStatus::Submitted {
                    match self.profitability(proposal, &record, gas).await {
                        Some(Ok(())) => { /* profitable — activate */ }
                        Some(Err(reason)) => return Some(Verdict::Reject(reason)),
                        None => return None,
                    }
                }
                Some(Verdict::Accept(Some(SimulationOutcome {
                    gas_used: gas,
                    trampoline,
                    sell_token: record.order.sell_token,
                    buy_token: record.order.buy_token,
                })))
            }
            Err(e) if is_revert(&e) => {
                tracing::info!(
                    id = %proposal.id,
                    sub_solver = %proposal.sub_solver,
                    error = %e,
                    "simulation reverted",
                );
                Some(Verdict::SimFailed)
            }
            Err(e) => {
                tracing::warn!(
                    id = %proposal.id,
                    sub_solver = %proposal.sub_solver,
                    error = %e,
                    "simulation failed (transient), deferring to next tick",
                );
                None
            }
        }
    }
}

/// Returns `true` when the RPC response indicates an EVM execution revert.
/// Only error code `3` (the Ethereum JSON-RPC "execution reverted" code) is
/// treated as a definitive revert. Other `ErrorResp` codes (rate limiting,
/// gas caps, server errors) are transient and should be deferred.
fn is_revert(e: &alloy::transports::RpcError<alloy::transports::TransportErrorKind>) -> bool {
    match e {
        RpcError::ErrorResp(payload) => payload.code == 3,
        RpcError::NullResp => true,
        _ => false,
    }
}

/// Returns `true` when a trampoline resolution error is a real failure
/// (contract revert) rather than a transient transport error. Same
/// classification as [`is_revert`] but operating on `alloy::contract::Error`
/// (which wraps the transport layer).
fn is_trampoline_revert(e: &alloy::contract::Error) -> bool {
    match e {
        alloy::contract::Error::TransportError(rpc_err) => is_revert(rpc_err),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// ProposalValidator (composite)
// ---------------------------------------------------------------------------

/// The production validator: runs [`EscrowValidator`] first (cheap cached
/// read), then [`SimulationValidator`] (expensive `eth_estimateGas`).
/// Short-circuits on the first non-`Accept` verdict.
pub struct ProposalValidator<P, O> {
    escrow: EscrowValidator<P>,
    simulation: SimulationValidator<P, O>,
}

impl<P: Provider, O: FetchOrder> ProposalValidator<P, O> {
    pub fn new(escrow: EscrowValidator<P>, simulation: SimulationValidator<P, O>) -> Self {
        Self { escrow, simulation }
    }
}

impl<P: Provider + Send + Sync, O: FetchOrder> ValidateProposal for ProposalValidator<P, O> {
    fn begin_tick(&self) {
        self.escrow.begin_tick();
        // Simulation trampoline cache is persistent — no per-tick clearing.
    }

    async fn validate(&self, proposal: &Proposal) -> Option<Verdict> {
        // 1. Escrow check (cheap, cached).
        let escrow_verdict = self.escrow.validate(proposal).await;
        match escrow_verdict {
            Some(Verdict::Accept(_)) => { /* continue to simulation */ }
            _ => return escrow_verdict, // Reject, SimFailed, or None (deferred)
        }

        // 2. Simulation (expensive, RPC).
        self.simulation.validate(proposal).await
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::domain::{
            order::{OrderRecord, test_order_record},
            proposal::{OrderUid, ProposalStatus, test_proposal},
        },
        alloy::primitives::{B256, address, hex},
        wiremock::{Mock, MockServer, ResponseTemplate, matchers::method},
    };

    // -----------------------------------------------------------------------
    // Test doubles
    // -----------------------------------------------------------------------

    /// Order source stub: always answers with the same result.
    enum StubOrders {
        Found(Box<OrderRecord>),
        NotFound,
        Transient,
        /// The order fetch succeeds but the native-price fetch is down.
        PriceOutage(Box<OrderRecord>),
        /// Only the given token has a price; any other lookup is `NotFound`.
        /// Pins which token the profitability gate prices.
        PricedOnly(Box<OrderRecord>, Address),
    }

    impl FetchOrder for StubOrders {
        async fn order(&self, _uid: &OrderUid) -> Result<OrderRecord, OrderbookError> {
            match self {
                Self::Found(record) | Self::PriceOutage(record) | Self::PricedOnly(record, _) => {
                    Ok((**record).clone())
                }
                Self::NotFound => Err(OrderbookError::NotFound),
                Self::Transient => Err(OrderbookError::Transient("boom".into())),
            }
        }

        /// Parity pricing: 10^18 wei per 10^18 atoms, so surplus in token
        /// units equals surplus in wei.
        async fn native_price(&self, token: Address) -> Result<U256, OrderbookError> {
            match self {
                Self::PriceOutage(_) => Err(OrderbookError::Transient("price boom".into())),
                Self::PricedOnly(_, priced) if *priced != token => Err(OrderbookError::NotFound),
                _ => Ok(alloy::primitives::utils::Unit::ETHER.wei()),
            }
        }
    }

    const SETTLEMENT: Address = address!("9008D19f58AAbD9eD0D60971565AA8510560ab41");
    const ESCROW: Address = address!("00000000000000000000000000000000000000EE");
    const FACTORY: Address = address!("0000000000000000000000000000000000000042");
    const TRAMPOLINE: Address = address!("0000000000000000000000000000000000000099");
    const AUTHENTICATOR: Address = address!("0000000000000000000000000000000000000077");

    /// Fake JSON-RPC node: answers `eth_call` address getters (factory →
    /// trampoline, settlement → authenticator) and `eth_estimateGas` with
    /// 200_000 gas.
    struct RpcResponder;

    impl wiremock::Respond for RpcResponder {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let result = match body["method"].as_str().unwrap_or_default() {
                "eth_estimateGas" => serde_json::json!("0x30d40"),
                "eth_call" => {
                    let to = body["params"][0]["to"]
                        .as_str()
                        .unwrap_or_default()
                        .to_lowercase();
                    let addr = if to == format!("{FACTORY:#x}") {
                        TRAMPOLINE
                    } else {
                        AUTHENTICATOR
                    };
                    serde_json::json!(format!(
                        "0x{}",
                        hex::encode(B256::left_padding_from(addr.as_slice()))
                    ))
                }
                other => panic!("unexpected RPC method {other}"),
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": body["id"],
                "result": result,
            }))
        }
    }

    async fn rpc_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(RpcResponder)
            .mount(&server)
            .await;
        server
    }

    fn validator_with(
        uri: String,
        orderbook: StubOrders,
    ) -> SimulationValidator<impl Provider, StubOrders> {
        validator_with_gas_price(uri, orderbook, 0)
    }

    fn validator_with_gas_price(
        uri: String,
        orderbook: StubOrders,
        gas_price: u64,
    ) -> SimulationValidator<impl Provider, StubOrders> {
        let provider = alloy::providers::ProviderBuilder::new().connect_http(uri.parse().unwrap());
        SimulationValidator::new(
            provider,
            orderbook,
            SETTLEMENT,
            ESCROW,
            FACTORY,
            Arc::new(AtomicU64::new(gas_price)),
            U256::ZERO,
        )
    }

    fn submitted_proposal() -> Proposal {
        test_proposal(
            OrderUid([0xaa; 56]),
            address!("0000000000000000000000000000000000000001"),
            ProposalStatus::Submitted,
        )
    }

    // -----------------------------------------------------------------------
    // SimulationValidator
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn simulation_dispatches_full_settle_with_overrides() {
        let server = rpc_server().await;
        let validator = validator_with(
            server.uri(),
            StubOrders::Found(Box::new(test_order_record())),
        );

        let verdict = validator.validate(&submitted_proposal()).await;
        assert_eq!(
            verdict,
            Some(Verdict::Accept(Some(SimulationOutcome {
                gas_used: 200_000,
                trampoline: TRAMPOLINE,
                sell_token: test_order_record().order.sell_token,
                buy_token: test_order_record().order.buy_token,
            }))),
        );

        // Inspect the eth_estimateGas request that went over the wire.
        let requests = server.received_requests().await.unwrap();
        let estimate = requests
            .iter()
            .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
            .find(|b| b["method"] == "eth_estimateGas")
            .expect("an eth_estimateGas request should be sent");
        let params = estimate["params"].as_array().unwrap();

        // The transaction: dummy submitter → settlement, carrying the exact
        // settle() calldata the encoder produces for these inputs.
        let tx = &params[0];
        assert_eq!(
            tx["from"].as_str().unwrap().to_lowercase(),
            format!("{:#x}", simulation::DUMMY_SUBMITTER),
        );
        assert_eq!(
            tx["to"].as_str().unwrap().to_lowercase(),
            format!("{SETTLEMENT:#x}"),
        );
        let proposal = submitted_proposal();
        let expected_calldata = byos_common::settlement::encode_settle(
            &test_order_record().order,
            &byos_common::contracts::Proposal {
                orderUidHash: proposal.order_uid_hash,
                sellAmount: proposal.sell_amount,
                buyAmount: proposal.buy_amount,
                validUntil: proposal.valid_until,
                nonce: proposal.nonce,
            },
            TRAMPOLINE,
            &proposal.interactions,
            &proposal.signature,
        );
        let input = tx
            .get("input")
            .or_else(|| tx.get("data"))
            .and_then(|v| v.as_str())
            .expect("tx should carry calldata");
        assert_eq!(input, format!("0x{}", hex::encode(&expected_calldata)));

        // The state overrides: AnyoneAuthenticator code at the authenticator,
        // SUBMITTER_ROLE state_diff on the escrow at the pinned slot-5 slot.
        let overrides = params
            .iter()
            .find_map(|p| {
                p.as_object().and_then(|obj| {
                    obj.iter()
                        .any(|(k, _)| k.to_lowercase() == format!("{AUTHENTICATOR:#x}"))
                        .then_some(obj)
                })
            })
            .expect("state overrides should be sent");
        let by_addr = |addr: Address| {
            overrides
                .iter()
                .find(|(k, _)| k.to_lowercase() == format!("{addr:#x}"))
                .map(|(_, v)| v)
                .unwrap()
        };
        assert!(
            by_addr(AUTHENTICATOR)["code"]
                .as_str()
                .unwrap()
                .starts_with("0x60806040"),
            "authenticator override should inject code",
        );
        let escrow_diff = &by_addr(ESCROW)["stateDiff"];
        let slot = "0x4eb8c5e0e8f6947fc61867e46604b89f6f2511c7f24d1be62be922d32b056655";
        let value = escrow_diff
            .as_object()
            .unwrap()
            .iter()
            .find(|(k, _)| k.to_lowercase() == slot)
            .map(|(_, v)| v.as_str().unwrap())
            .expect("escrow override should write the SUBMITTER_ROLE slot");
        assert!(value.ends_with('1'), "role slot should be set to true");
    }

    #[tokio::test]
    async fn unprofitable_first_simulation_rejects_proposal() {
        let server = rpc_server().await;
        // Order limit equal to the proposal's buy amount: zero surplus, so
        // the score cannot exceed zero no matter the gas price.
        let mut record = test_order_record();
        record.order.buy_amount = submitted_proposal().buy_amount;
        let validator = validator_with(server.uri(), StubOrders::Found(Box::new(record)));

        let verdict = validator.validate(&submitted_proposal()).await;
        assert_eq!(
            verdict,
            Some(Verdict::Reject(RejectionReason::Unprofitable)),
        );
    }

    #[tokio::test]
    async fn buy_order_gate_prices_the_sell_token() {
        let server = rpc_server().await;
        // A buy order around the same proposal: exact buy amount (the
        // envelope requirement), sell limit above the proposal's 1_000_000 so
        // the pair carries 100_000 surplus — in the sell token.
        let mut record = test_order_record();
        record.order.kind = OrderKind::Buy;
        record.order.buy_amount = submitted_proposal().buy_amount;
        record.order.sell_amount = U256::from(1_100_000_u64);
        // Only the sell token is priced: if the gate wrongly priced the buy
        // token it would see NotFound and reject as Unprofitable.
        let sell_token = record.order.sell_token;
        let validator = validator_with(
            server.uri(),
            StubOrders::PricedOnly(Box::new(record), sell_token),
        );

        let verdict = validator.validate(&submitted_proposal()).await;
        assert!(
            matches!(verdict, Some(Verdict::Accept(Some(_)))),
            "buy-order surplus must be priced in the sell token, got {verdict:?}",
        );
    }

    #[tokio::test]
    async fn revalidation_of_active_proposal_skips_the_profitability_gate() {
        // 1 gwei: the simulated 200k gas costs ~2e14 wei, dwarfing the
        // 10_000-wei surplus at parity pricing — the score is deeply negative.
        let gas_price = 1_000_000_000;

        // The gate would reject these inputs on a first (Submitted) pass…
        let server = rpc_server().await;
        let validator = validator_with_gas_price(
            server.uri(),
            StubOrders::Found(Box::new(test_order_record())),
            gas_price,
        );
        let verdict = validator.validate(&submitted_proposal()).await;
        assert_eq!(
            verdict,
            Some(Verdict::Reject(RejectionReason::Unprofitable)),
            "sanity: these inputs must be unprofitable at this gas price",
        );

        // …but re-validation of an Active proposal must not churn it: the
        // simulation still runs (gas refresh), the gate is skipped.
        let validator = validator_with_gas_price(
            server.uri(),
            StubOrders::Found(Box::new(test_order_record())),
            gas_price,
        );
        let mut active = submitted_proposal();
        active.status = ProposalStatus::Active;
        let verdict = validator.validate(&active).await;
        assert!(
            matches!(verdict, Some(Verdict::Accept(Some(_)))),
            "gas-price wobble must not reject an Active proposal, got {verdict:?}",
        );
    }

    #[tokio::test]
    async fn native_price_outage_defers_first_verdict() {
        let server = rpc_server().await;
        let validator = validator_with(
            server.uri(),
            StubOrders::PriceOutage(Box::new(test_order_record())),
        );

        let verdict = validator.validate(&submitted_proposal()).await;
        assert_eq!(
            verdict, None,
            "a transient price failure must defer, not reject or activate",
        );
    }

    #[tokio::test]
    async fn unknown_order_rejects_proposal() {
        let server = rpc_server().await;
        let validator = validator_with(server.uri(), StubOrders::NotFound);

        let verdict = validator.validate(&submitted_proposal()).await;
        assert_eq!(
            verdict,
            Some(Verdict::Reject(RejectionReason::OrderNotFound)),
        );
    }

    #[tokio::test]
    async fn orderbook_outage_defers_judgment() {
        let server = rpc_server().await;
        let validator = validator_with(server.uri(), StubOrders::Transient);

        let verdict = validator.validate(&submitted_proposal()).await;
        assert_eq!(verdict, None);
    }

    #[tokio::test]
    async fn out_of_envelope_order_rejects_proposal() {
        let server = rpc_server().await;
        let mut record = test_order_record();
        record.has_hooks = true;
        let validator = validator_with(server.uri(), StubOrders::Found(Box::new(record)));

        let verdict = validator.validate(&submitted_proposal()).await;
        assert_eq!(
            verdict,
            Some(Verdict::Reject(RejectionReason::UnsupportedOrder)),
        );
    }

    #[tokio::test]
    async fn simulation_returns_none_on_transport_error() {
        // Provider pointed at a port that is (almost certainly) not listening.
        let validator = validator_with(
            "http://127.0.0.1:1".to_string(),
            StubOrders::Found(Box::new(test_order_record())),
        );

        let verdict = validator.validate(&submitted_proposal()).await;
        assert_eq!(verdict, None, "transport error should defer judgment");
    }

    #[test]
    fn trampoline_cache_returns_stored_address() {
        let provider = alloy::providers::ProviderBuilder::new()
            .connect_http("http://127.0.0.1:1".parse().unwrap());
        let validator = SimulationValidator::new(
            provider,
            StubOrders::NotFound,
            Address::ZERO,
            Address::ZERO,
            Address::ZERO,
            Arc::new(AtomicU64::new(0)),
            U256::ZERO,
        );

        let sub_solver = address!("0000000000000000000000000000000000000001");
        let trampoline = address!("0000000000000000000000000000000000000099");

        // Pre-populate cache.
        validator
            .trampoline_cache
            .lock()
            .insert(sub_solver, trampoline);

        // Verify cache hit (sync check, no RPC needed).
        let cached = validator.trampoline_cache.lock().get(&sub_solver).copied();
        assert_eq!(cached, Some(trampoline));
    }

    #[test]
    fn is_revert_classifies_null_resp_as_revert() {
        assert!(is_revert(&RpcError::NullResp));
    }

    #[test]
    fn is_revert_classifies_code_3_as_revert() {
        let payload = alloy::rpc::json_rpc::ErrorPayload {
            code: 3,
            message: "execution reverted".into(),
            data: None,
        };
        assert!(is_revert(&RpcError::ErrorResp(payload)));
    }

    #[test]
    fn is_revert_defers_rate_limit_error() {
        let payload = alloy::rpc::json_rpc::ErrorPayload {
            code: 429,
            message: "rate limit exceeded".into(),
            data: None,
        };
        assert!(!is_revert(&RpcError::ErrorResp(payload)));
    }

    #[test]
    fn is_revert_classifies_transport_error_as_not_revert() {
        let transport = alloy::transports::TransportErrorKind::custom(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ));
        assert!(!is_revert(&transport));
    }

    // -----------------------------------------------------------------------
    // is_trampoline_revert
    // -----------------------------------------------------------------------

    #[test]
    fn trampoline_null_resp_is_revert() {
        let err = alloy::contract::Error::TransportError(RpcError::NullResp);
        assert!(is_trampoline_revert(&err));
    }

    #[test]
    fn trampoline_transport_error_is_not_revert() {
        let err =
            alloy::contract::Error::TransportError(alloy::transports::TransportErrorKind::custom(
                std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
            ));
        assert!(!is_trampoline_revert(&err));
    }
}
