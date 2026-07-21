//! `/solve` hot path: served entirely from the in-memory proposal cache.
//! Zero simulation, zero RPC, zero DB on this path (ADR-0002).

use {
    super::AppState,
    crate::domain::{
        proposal::{OrderUid, Proposal},
        scoring::{ScoreInput, score_proposal},
    },
    alloy::primitives::{Address, U256},
    axum::{Json, extract::State},
    byos_common::trampoline::encode_trampoline_interactions,
    solvers_dto::{
        auction::{self, Auction},
        solution::{self, Solutions},
    },
    std::{
        collections::HashMap,
        sync::atomic::Ordering,
        time::{SystemTime, UNIX_EPOCH},
    },
};

/// Fixed gas estimate for M1 (no simulation-based estimate yet).
const M1_GAS_ESTIMATE: u64 = 200_000;

/// POST /solve — the driver-facing solver engine endpoint.
pub async fn solve(State(state): State<AppState>, Json(auction): Json<Auction>) -> Json<Solutions> {
    // Publish the auction's gas price so the background escrow validator uses
    // a fresh value instead of the startup fallback.
    let gp: u64 = auction.effective_gas_price.try_into().unwrap_or(u64::MAX);
    state.gas_price().store(gp, Ordering::Relaxed);

    let mut solutions = Vec::new();
    let now = U256::from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    );

    for order in &auction.orders {
        let order_uid = OrderUid(order.uid);

        let proposals = state.store().list_by_order_uid(&order_uid);
        if proposals.is_empty() {
            continue;
        }

        let is_sell = matches!(order.kind, auction::Kind::Sell);
        let gas_cost = U256::from(M1_GAS_ESTIMATE).saturating_mul(auction.effective_gas_price);

        // The surplus token is the buy token for sell orders, sell token for buy
        // orders.
        let surplus_token = if is_sell {
            order.buy_token
        } else {
            order.sell_token
        };
        let native_price = auction
            .tokens
            .get(&surplus_token)
            .and_then(|t| t.reference_price)
            .unwrap_or(U256::ZERO);

        // Score and select the best proposal for this order.
        let best = proposals
            .iter()
            .filter(|p| p.valid_until > now)
            .filter_map(|p| {
                let score = score_proposal(&ScoreInput {
                    order_sell: order.sell_amount,
                    order_buy: order.buy_amount,
                    proposal_sell: p.sell_amount,
                    proposal_buy: p.buy_amount,
                    is_sell_order: is_sell,
                    gas_cost,
                    native_price,
                })?;
                (score > U256::ZERO).then_some((p, score))
            })
            .max_by_key(|(_, score)| *score);

        let Some((proposal, _score)) = best else {
            continue;
        };

        // Build the solution using solvers-dto types.
        let id = solutions.len() as u64 + 1;
        solutions.push(build_solution(id, order, proposal));
    }

    tracing::debug!(count = solutions.len(), "solve: returning solutions");

    Json(Solutions { solutions })
}

fn build_solution(id: u64, order: &auction::Order, proposal: &Proposal) -> solution::Solution {
    // We need a trampoline address to encode interactions. For M1, we use
    // Address::ZERO as a placeholder — in production this comes from
    // ITrampolineFactory.addressOf(subSolver) resolved at proposal ingestion.
    // TODO: resolve trampoline address during ingestion.
    let trampoline = Address::ZERO;

    let trampoline_interactions = encode_trampoline_interactions(
        trampoline,
        order.sell_token,
        &byos_common::contracts::Proposal {
            orderUidHash: proposal.order_uid_hash,
            sellAmount: proposal.sell_amount,
            buyAmount: proposal.buy_amount,
            validUntil: proposal.valid_until,
            nonce: proposal.nonce,
        },
        &proposal.interactions,
        order.buy_token,
        &proposal.signature,
    );

    // Convert byos-common Interactions to solvers-dto CustomInteractions.
    let interactions: Vec<solution::Interaction> = trampoline_interactions
        .iter()
        .map(|i| {
            solution::Interaction::Custom(solution::CustomInteraction {
                internalize: false,
                target: i.target,
                value: i.value,
                calldata: i.callData.to_vec(),
                allowances: vec![],
                inputs: vec![],
                outputs: vec![],
            })
        })
        .collect();

    // Clearing prices: cross-multiplied from the proposal amounts.
    let mut prices = HashMap::new();
    prices.insert(order.sell_token, proposal.buy_amount);
    prices.insert(order.buy_token, proposal.sell_amount);

    // Trade fulfillment.
    let executed_amount = if matches!(order.kind, auction::Kind::Sell) {
        proposal.sell_amount
    } else {
        proposal.buy_amount
    };

    let trade = solution::Trade::Fulfillment(solution::Fulfillment {
        order: solution::OrderUid(order.uid),
        executed_amount,
        fee: None,
    });

    solution::Solution {
        id,
        prices,
        trades: vec![trade],
        pre_interactions: vec![],
        interactions,
        post_interactions: vec![],
        gas: Some(M1_GAS_ESTIMATE),
        flashloans: None,
        wrappers: vec![],
    }
}
