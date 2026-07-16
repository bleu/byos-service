//! Proposal scoring: `score = surplus + fee - gas` (ADR-0002).
//!
//! The `/solve` hot path uses this to select the single highest-scoring
//! proposal per order UID. All computation is in-memory — no RPC, no DB.

use alloy::primitives::U256;

/// Score a proposal against an order. Returns the score as a signed value
/// (positive = profitable, negative = unprofitable).
///
/// `surplus` is the improvement over the order's limit:
///  - Sell order: `proposal_buy - order_buy` (more buy tokens for the user)
///  - Buy order: `order_sell - proposal_sell` (fewer sell tokens from the user)
///
/// For M1, `fee` is zero and `gas_cost` is a fixed estimate.
pub fn score_proposal(
    order_sell: U256,
    order_buy: U256,
    proposal_sell: U256,
    proposal_buy: U256,
    is_sell_order: bool,
    gas_cost: U256,
) -> Option<U256> {
    let surplus = if is_sell_order {
        // Sell order: user offers sell_amount, wants at least buy_amount.
        // Surplus = how much more buyToken the proposal provides.
        proposal_buy.checked_sub(order_buy)?
    } else {
        // Buy order: user wants buy_amount, offers at most sell_amount.
        // Surplus = how much less sellToken the proposal consumes.
        order_sell.checked_sub(proposal_sell)?
    };

    // score = surplus - gas_cost (fee is 0 in M1)
    surplus.checked_sub(gas_cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sell_order_positive_surplus() {
        let score = score_proposal(
            U256::from(1_000u64), // order sell
            U256::from(900u64),   // order buy (minimum)
            U256::from(1_000u64), // proposal sell
            U256::from(950u64),   // proposal buy (better than minimum)
            true,                 // sell order
            U256::from(10u64),    // gas cost
        );
        // surplus = 950 - 900 = 50, score = 50 - 10 = 40
        assert_eq!(score, Some(U256::from(40u64)));
    }

    #[test]
    fn buy_order_positive_surplus() {
        let score = score_proposal(
            U256::from(1_000u64), // order sell (maximum)
            U256::from(900u64),   // order buy
            U256::from(950u64),   // proposal sell (less than max)
            U256::from(900u64),   // proposal buy
            false,                // buy order
            U256::from(10u64),
        );
        // surplus = 1000 - 950 = 50, score = 50 - 10 = 40
        assert_eq!(score, Some(U256::from(40u64)));
    }

    #[test]
    fn proposal_below_minimum_returns_none() {
        let score = score_proposal(
            U256::from(1_000u64),
            U256::from(900u64),
            U256::from(1_000u64),
            U256::from(800u64), // below order's buy minimum
            true,
            U256::ZERO,
        );
        assert_eq!(score, None);
    }

    #[test]
    fn gas_exceeds_surplus_returns_none() {
        let score = score_proposal(
            U256::from(1_000u64),
            U256::from(900u64),
            U256::from(1_000u64),
            U256::from(910u64), // surplus = 10
            true,
            U256::from(20u64), // gas = 20 > surplus
        );
        assert_eq!(score, None);
    }

    #[test]
    fn zero_surplus_minus_zero_gas() {
        let score = score_proposal(
            U256::from(1_000u64),
            U256::from(900u64),
            U256::from(1_000u64),
            U256::from(900u64), // exactly at minimum
            true,
            U256::ZERO,
        );
        assert_eq!(score, Some(U256::ZERO));
    }
}
