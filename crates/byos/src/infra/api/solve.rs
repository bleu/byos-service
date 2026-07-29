//! `/solve` hot path: one indexed read over the live proposal rows per
//! auction order (ADR-0013 — no cache layer until latency data says
//! otherwise). Zero simulation, zero RPC on this path (ADR-0002).

use {
    super::AppState,
    crate::domain::{
        proposal::{OrderUid, Proposal},
        scoring::{ScoreInput, effective_gas, score_proposal, surplus_token},
    },
    alloy::primitives::U256,
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
            .expect("system clock before Unix epoch")
            .as_secs(),
    );

    for order in &auction.orders {
        let order_uid = OrderUid(order.uid);

        let proposals = match state.store().list_by_order_uid(&order_uid).await {
            Ok(proposals) => proposals,
            Err(e) => {
                // Skip this order rather than fail the whole auction — a
                // missing bid loses one round, a 500 loses them all.
                tracing::error!(%e, order_uid = %order_uid, "solve: proposal lookup failed");
                continue;
            }
        };
        if proposals.is_empty() {
            continue;
        }

        let is_sell = matches!(order.kind, auction::Kind::Sell);

        let native_price = auction
            .tokens
            .get(&surplus_token(is_sell, order.sell_token, order.buy_token))
            .and_then(|t| t.reference_price)
            .unwrap_or(U256::ZERO);

        // Score and select the best proposal for this order. Only proposals
        // with simulation gas are eligible — proposals that haven't been
        // simulated yet (gas_used: None) are skipped.
        let best = proposals
            .iter()
            .filter(|p| p.valid_until > now)
            .filter_map(|p| {
                let gas_used = p.gas_used?;
                let gas_cost =
                    U256::from(effective_gas(gas_used)).saturating_mul(auction.effective_gas_price);
                let score = score_proposal(&ScoreInput {
                    order_sell: order.sell_amount,
                    order_buy: order.buy_amount,
                    proposal_sell: p.sell_amount,
                    proposal_buy: p.buy_amount,
                    is_sell_order: is_sell,
                    gas_cost,
                    native_price,
                })?;
                (score > U256::ZERO).then_some((p, gas_used, score))
            })
            .max_by_key(|(_, _, score)| *score);

        let Some((proposal, gas_used, _score)) = best else {
            continue;
        };

        // Build the solution using solvers-dto types.
        let id = solutions.len() as u64 + 1;
        let Some(sol) = build_solution(id, order, proposal, gas_used) else {
            continue;
        };

        // Record notification attribution before bidding (ADR-0013): if we
        // can't record it, we don't bid it. Auctions without an id (quote
        // requests) are never settled, so there is nothing to attribute.
        if let Some(auction_id) = auction.id
            && let Err(e) = state
                .store()
                .record_solution(auction_id, id, proposal.id)
                .await
        {
            tracing::error!(
                %e, proposal_id = %proposal.id,
                "solve: solution not recorded, dropping the bid"
            );
            continue;
        }
        solutions.push(sol);
    }

    tracing::debug!(count = solutions.len(), "solve: returning solutions");

    Json(Solutions { solutions })
}

fn build_solution(
    id: u64,
    order: &auction::Order,
    proposal: &Proposal,
    gas_used: u64,
) -> Option<solution::Solution> {
    let Some(trampoline) = proposal.trampoline else {
        tracing::error!(
            id = %proposal.id,
            "proposal reached build_solution without trampoline — skipping",
        );
        return None;
    };

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

    Some(solution::Solution {
        id,
        prices,
        trades: vec![trade],
        pre_interactions: vec![],
        interactions,
        post_interactions: vec![],
        gas: Some(effective_gas(gas_used)),
        flashloans: None,
        wrappers: vec![],
    })
}
