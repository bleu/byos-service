//! Proposal scoring: `score = surplus + fee - gas` (ADR-0002).
//!
//! The `/solve` hot path uses this to select the single highest-scoring
//! proposal per order UID. All computation is in-memory — no RPC, no DB.

use alloy::primitives::{U256, utils::Unit};

/// Conservative gas floor for escrow threshold calculations. Not used for
/// scoring — `/solve` uses the actual simulated gas from each proposal.
pub const ESCROW_GAS_ESTIMATION: u64 = 200_000;

/// Buffer added to simulated gas for scoring: `gas = simulated_gas + GAS_BUFFER`.
pub const GAS_BUFFER: u64 = 100_000;

pub struct ScoreInput {
    pub order_sell: U256,
    pub order_buy: U256,
    pub proposal_sell: U256,
    pub proposal_buy: U256,
    pub is_sell_order: bool,
    /// Gas cost in wei (`gas_estimate × effective_gas_price`).
    pub gas_cost: U256,
    /// Auction reference price for the surplus token: how much wei buys 10^18
    /// atoms of that token. Converts surplus to native-token units.
    pub native_price: U256,
}

/// Score a proposal against an order. Returns `None` when the proposal is
/// below the order's limit or when gas exceeds the surplus.
///
/// Surplus is the improvement over the order's limit:
///  - Sell order: `proposal_buy - order_buy` (more buy tokens for the user)
///  - Buy order: `order_sell - proposal_sell` (fewer sell tokens from the user)
///
/// For M1, `fee` is zero and `gas_cost` is a fixed estimate.
pub fn score_proposal(input: &ScoreInput) -> Option<U256> {
    let surplus = if input.is_sell_order {
        // Sell order: user offers sell_amount, wants at least buy_amount.
        // Surplus = how much more buyToken the proposal provides.
        input.proposal_buy.checked_sub(input.order_buy)?
    } else {
        // Buy order: user wants buy_amount, offers at most sell_amount.
        // Surplus = how much less sellToken the proposal consumes.
        input.order_sell.checked_sub(input.proposal_sell)?
    };

    // Convert surplus from token units to native-token (wei) units.
    let surplus_eth = surplus
        .checked_mul(input.native_price)?
        .checked_div(Unit::ETHER.wei())?;

    // score = surplus_eth - gas_cost (fee is 0 in M1)
    surplus_eth.checked_sub(input.gas_cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sell_order_positive_surplus() {
        // Surplus token (buy token) is worth 0.5 ETH per 10^18 atoms.
        let score = score_proposal(&ScoreInput {
            order_sell: U256::from(1_000u64),
            order_buy: U256::from(900u64),
            proposal_sell: U256::from(1_000u64),
            proposal_buy: U256::from(950u64),
            is_sell_order: true,
            gas_cost: U256::ZERO,
            native_price: Unit::ETHER.wei() / U256::from(2),
        });
        // surplus = 950 - 900 = 50
        // surplus_eth = 50 * 0.5e18 / 1e18 = 25
        assert_eq!(score, Some(U256::from(25u64)));
    }

    #[test]
    fn buy_order_positive_surplus() {
        let score = score_proposal(&ScoreInput {
            order_sell: U256::from(1_000u64),
            order_buy: U256::from(900u64),
            proposal_sell: U256::from(950u64),
            proposal_buy: U256::from(900u64),
            is_sell_order: false,
            gas_cost: U256::ZERO,
            native_price: Unit::ETHER.wei() / U256::from(2),
        });
        // surplus = 1000 - 950 = 50
        // surplus_eth = 50 * 0.5e18 / 1e18 = 25
        assert_eq!(score, Some(U256::from(25u64)));
    }

    #[test]
    fn proposal_below_minimum_returns_none() {
        let score = score_proposal(&ScoreInput {
            order_sell: U256::from(1_000u64),
            order_buy: U256::from(900u64),
            proposal_sell: U256::from(1_000u64),
            proposal_buy: U256::from(800u64), // below order's buy minimum
            is_sell_order: true,
            gas_cost: U256::ZERO,
            native_price: Unit::ETHER.wei(),
        });
        assert_eq!(score, None);
    }

    #[test]
    fn gas_exceeds_surplus_returns_none() {
        // Native price = 1:1 so surplus_eth equals surplus in token units.
        let score = score_proposal(&ScoreInput {
            order_sell: U256::from(1_000u64),
            order_buy: U256::from(900u64),
            proposal_sell: U256::from(1_000u64),
            proposal_buy: U256::from(910u64), // surplus = 10
            is_sell_order: true,
            gas_cost: U256::from(20u64), // gas = 20 > surplus_eth (10)
            native_price: Unit::ETHER.wei(),
        });
        assert_eq!(score, None);
    }

    #[test]
    fn zero_surplus_minus_zero_gas() {
        let score = score_proposal(&ScoreInput {
            order_sell: U256::from(1_000u64),
            order_buy: U256::from(900u64),
            proposal_sell: U256::from(1_000u64),
            proposal_buy: U256::from(900u64), // exactly at minimum
            is_sell_order: true,
            gas_cost: U256::ZERO,
            native_price: Unit::ETHER.wei(),
        });
        assert_eq!(score, Some(U256::ZERO));
    }
}
