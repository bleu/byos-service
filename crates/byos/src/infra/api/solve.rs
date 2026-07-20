//! `/solve` hot path: served entirely from the in-memory proposal cache.
//! Zero simulation, zero RPC, zero DB on this path (ADR-0002).

use {
    super::AppState,
    crate::domain::{
        proposal::{OrderUid, Proposal},
        scoring::score_proposal,
    },
    alloy::primitives::{Address, U256},
    axum::{Json, extract::State},
    byos_common::trampoline::encode_trampoline_interactions,
    solvers_dto::{
        auction::{self, Auction},
        solution::{self, Solutions},
    },
    std::collections::HashMap,
};

/// Fixed gas estimate for M1 (no simulation-based estimate yet).
const M1_GAS_ESTIMATE: u64 = 200_000;

/// POST /solve — the driver-facing solver engine endpoint.
pub async fn solve(State(state): State<AppState>, Json(auction): Json<Auction>) -> Json<Solutions> {
    let mut solutions = Vec::new();
    let mut solution_id: u64 = 0;

    for order in &auction.orders {
        let order_uid = OrderUid(order.uid);

        let proposals = state.store().list_by_order_uid(&order_uid);
        if proposals.is_empty() {
            continue;
        }

        let is_sell = matches!(order.kind, auction::Kind::Sell);
        let gas_cost = U256::from(M1_GAS_ESTIMATE).saturating_mul(auction.effective_gas_price);

        // The surplus token is the buy token for sell orders, sell token for buy orders.
        let surplus_token = if is_sell { order.buy_token } else { order.sell_token };
        let native_price = auction
            .tokens
            .get(&surplus_token)
            .and_then(|t| t.reference_price)
            .unwrap_or(U256::ZERO);

        // Score and select the best proposal for this order.
        let best = proposals
            .iter()
            .filter_map(|p| {
                let score = score_proposal(
                    order.sell_amount,
                    order.buy_amount,
                    p.sell_amount,
                    p.buy_amount,
                    is_sell,
                    gas_cost,
                    native_price,
                )?;
                Some((p, score))
            })
            .max_by_key(|(_, score)| *score);

        let Some((proposal, _score)) = best else {
            continue;
        };

        // Build the solution using solvers-dto types.
        if let Some(sol) = build_solution(&mut solution_id, order, proposal) {
            solutions.push(sol);
        }
    }

    tracing::debug!(count = solutions.len(), "solve: returning solutions");

    Json(Solutions { solutions })
}

fn build_solution(
    id_counter: &mut u64,
    order: &auction::Order,
    proposal: &Proposal,
) -> Option<solution::Solution> {
    // We need a trampoline address to encode interactions. For M1, we use
    // Address::ZERO as a placeholder — in production this comes from
    // ITrampolineFactory.addressOf(subSolver) resolved at proposal ingestion.
    // TODO(COW-1162): resolve trampoline address during ingestion simulation.
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

    *id_counter += 1;

    Some(solution::Solution {
        id: *id_counter,
        prices,
        trades: vec![trade],
        pre_interactions: vec![],
        interactions,
        post_interactions: vec![],
        gas: Some(M1_GAS_ESTIMATE),
        flashloans: None,
        wrappers: vec![],
    })
}
