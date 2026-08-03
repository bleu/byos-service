//! `/solve` hot path: one indexed read over the live proposal rows per auction
//! (ADR-0013 — no cache layer until latency data says otherwise). Zero
//! simulation, zero RPC on this path (ADR-0002).

use {
    super::AppState,
    crate::domain::{
        gas_cut::{self, GasCut, SellTokenPrice},
        proposal::{OrderUid, Proposal},
        scoring::{Candidate, SurplusPrice, effective_gas, score_proposal, surplus_token},
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

/// POST /solve — the driver-facing solver engine endpoint.
pub async fn solve(State(state): State<AppState>, Json(auction): Json<Auction>) -> Json<Solutions> {
    // Publish the auction's gas price so the background escrow validator uses
    // a fresh value instead of the startup fallback. A price that does not fit
    // leaves the previous value in place: saturating to u64::MAX would push
    // every sub-solver under the escrow threshold and reject the entire live
    // book on the next tick, and Rejected is terminal (ADR-0013).
    match u64::try_from(auction.effective_gas_price) {
        Ok(gp) => state.gas_price().store(gp, Ordering::Relaxed),
        Err(_) => tracing::warn!(
            effective_gas_price = %auction.effective_gas_price,
            "auction gas price does not fit u64; keeping the previous value"
        ),
    }

    let mut solutions = Vec::new();
    let now = U256::from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_secs(),
    );

    // One lookup for the whole auction. Per-order queries cost a round trip
    // each, nearly all of them returning nothing, and scaled with auction size
    // rather than with how many proposals we hold (ADR-0002's 100ms p99).
    let order_uids: Vec<OrderUid> = auction.orders.iter().map(|o| OrderUid(o.uid)).collect();
    let by_order = match state.store().active_by_order_uids(&order_uids).await {
        Ok(by_order) => by_order,
        Err(e) => {
            // Bid nothing rather than answer an error: losing this round is
            // recoverable, and the driver retries next auction.
            tracing::error!(%e, "solve: proposal lookup failed");
            return Json(Solutions { solutions: vec![] });
        }
    };

    for order in &auction.orders {
        let order_uid = OrderUid(order.uid);

        let Some(proposals) = by_order.get(&order_uid) else {
            continue;
        };

        let is_sell = matches!(order.kind, auction::Kind::Sell);

        let price_of = |token| {
            auction
                .tokens
                .get(&token)
                .and_then(|t| t.reference_price)
                .unwrap_or(U256::ZERO)
        };
        let surplus_price = SurplusPrice(price_of(surplus_token(
            is_sell,
            order.sell_token,
            order.buy_token,
        )));
        // A second lookup, not the same one: the cut is in the sell token, which
        // for a sell order is not where the surplus is.
        let sell_token_price = SellTokenPrice(price_of(order.sell_token));

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
                let candidate = Candidate {
                    order_sell: order.sell_amount,
                    order_buy: order.buy_amount,
                    proposal_sell: p.sell_amount,
                    proposal_buy: p.buy_amount,
                    is_sell_order: is_sell,
                    gas_cost,
                };
                let score = score_proposal(&candidate, surplus_price)?;
                // Can rule out a proposal the score accepts: the score converts
                // surplus at the auction's price, the limit is enforced on the
                // route's own amounts, and a stale price makes them disagree.
                // Dropping it here leaves the order to a runner-up.
                let cut = gas_cut::size(&candidate, sell_token_price)?;
                (score > U256::ZERO).then_some((p, gas_used, cut, score))
            })
            .max_by_key(|(_, _, _, score)| *score);

        let Some((proposal, gas_used, cut, _score)) = best else {
            continue;
        };

        // Build the solution using solvers-dto types.
        let id = solutions.len() as u64 + 1;
        let Some(sol) =
            build_solution(id, order, proposal, gas_used, cut, state.hooks_trampoline())
        else {
            continue;
        };

        // Record notification attribution before bidding (ADR-0013): if we
        // can't record it, we don't bid it. Auctions without an id (quote
        // requests) are never settled, so there is nothing to attribute.
        let solution_id = i64::try_from(id).expect("bounded by the auction's order count");
        if let Some(auction_id) = auction.id
            && let Err(e) = state
                .store()
                .record_solution(auction_id, solution_id, proposal.id)
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
    cut: GasCut,
    hooks_trampoline: Option<Address>,
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

    // Encode order hooks as pre/post interactions via HooksTrampoline.
    let (pre_raw, post_raw) = proposal.hooks.encode_interactions(hooks_trampoline);
    let to_calls = |encoded: Vec<byos_common::contracts::GPv2InteractionData>| -> Vec<solution::Call> {
        encoded
            .into_iter()
            .map(|i| solution::Call {
                target: i.target,
                value: i.value,
                calldata: i.callData.to_vec(),
            })
            .collect()
    };
    let pre_interactions = to_calls(pre_raw);
    let post_interactions = to_calls(post_raw);

    // Clearing prices: cross-multiplied from the proposal amounts, and left
    // alone by the cut, so `encode_settle` keeps producing the transaction we
    // simulated.
    let mut prices = HashMap::new();
    prices.insert(order.sell_token, proposal.buy_amount);
    prices.insert(order.buy_token, proposal.sell_amount);

    // The cut is never omitted: every live order is limit class, and the driver
    // rejects a solution that leaves the fee to the protocol on one of those.
    let trade = solution::Trade::Fulfillment(solution::Fulfillment {
        order: solution::OrderUid(order.uid),
        executed_amount: cut.executed_amount,
        fee: Some(cut.amount),
    });

    Some(solution::Solution {
        id,
        prices,
        trades: vec![trade],
        pre_interactions,
        interactions,
        post_interactions,
        gas: Some(effective_gas(gas_used)),
        flashloans: None,
        wrappers: vec![],
    })
}
