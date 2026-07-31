//! Proposal scoring: `score = surplus - gas` in native-token units (ADR-0002).
//!
//! The `/solve` hot path uses this to select the single highest-scoring
//! proposal per order UID. All computation is in-memory — no RPC, no DB.

use alloy::primitives::{Address, U256, utils::Unit};

/// Conservative gas floor for escrow threshold calculations. Not used for
/// scoring — `/solve` uses the actual simulated gas from each proposal.
pub const ESCROW_GAS_ESTIMATION: u64 = 200_000;

/// Buffer added to simulated gas for scoring: `gas = simulated_gas +
/// GAS_BUFFER`. Small by design: the full-settle simulation (ADR-0012)
/// already covers intrinsic gas and the whole settlement path, so the buffer
/// only absorbs warm/cold storage differences and driver batching variance.
pub const GAS_BUFFER: u64 = 30_000;

/// Effective gas for a simulated proposal: simulated gas + safety buffer.
///
/// Saturating because `simulated` is whatever the node returned from
/// `eth_estimateGas`. The release profile has no overflow checks, so a wrapped
/// sum would collapse `gas_cost` to near zero and score an absurd proposal as
/// the auction's best.
pub fn effective_gas(simulated: u64) -> u64 {
    simulated.saturating_add(GAS_BUFFER)
}

/// The token surplus is denominated in: the buy token for a sell order (the
/// user gets more buy tokens), the sell token for a buy order (the user keeps
/// more sell tokens). Shared by `/solve` and the profitability gate so the
/// two paths cannot drift.
pub fn surplus_token(is_sell_order: bool, sell_token: Address, buy_token: Address) -> Address {
    if is_sell_order { buy_token } else { sell_token }
}

/// The auction's reference price for the [`surplus_token`]: wei per 10^18
/// atoms. Its own type because the gas cut takes the sell token's price
/// instead ([`SellTokenPrice`](super::gas_cut::SellTokenPrice)), and on a sell
/// order those are two different numbers one argument apart.
#[derive(Clone, Copy, Debug)]
pub struct SurplusPrice(pub U256);

/// A proposal weighed against its order at this auction's gas price. Shared by
/// [`score_proposal`] and [`gas_cut::size`](super::gas_cut::size), which ask
/// different questions of the same pair and take different prices to do it.
///
/// Assumes the pair passed the validation envelope
/// ([`OrderRecord::check_envelope`](super::order::OrderRecord::check_envelope)):
/// a sell order's `proposal_sell` equals its `order_sell`, a buy order's
/// `proposal_buy` its `order_buy`. The cut's buy-side limit check leans on that
/// second equality. `/solve` does not re-check it — an `Active` proposal is one
/// that passed.
#[derive(Clone, Copy, Debug)]
pub struct Candidate {
    pub order_sell: U256,
    pub order_buy: U256,
    pub proposal_sell: U256,
    pub proposal_buy: U256,
    pub is_sell_order: bool,
    /// Gas cost in wei (`effective_gas(gas_used) × effective_gas_price`).
    pub gas_cost: U256,
}

/// Score a proposal against an order. Returns `None` when the proposal is
/// below the order's limit or when gas exceeds the surplus.
///
/// Surplus is the improvement over the order's limit:
///  - Sell order: `proposal_buy - order_buy` (more buy tokens for the user)
///  - Buy order: `order_sell - proposal_sell` (fewer sell tokens from the user)
///
/// No fee term, deliberately. CoW's score is surplus plus protocol fees and
/// nothing else (CIP-38), the protocol fee cancels out of any ranking, and the
/// [gas cut](super::gas_cut) reaches the score as a subtraction from surplus.
/// So `surplus - gas` is the autopilot's score for our bid, give or take the
/// route's price improvement over the auction's reference ratio: the cut is a
/// fixed number of sell-token atoms, and it displaces exactly its own worth in
/// surplus only when the route trades at that ratio. Reading per-order fee
/// policies would still buy nothing.
pub fn score_proposal(candidate: &Candidate, surplus_price: SurplusPrice) -> Option<U256> {
    let SurplusPrice(native_price) = surplus_price;
    let surplus = if candidate.is_sell_order {
        // Sell order: user offers sell_amount, wants at least buy_amount.
        // Surplus = how much more buyToken the proposal provides.
        candidate.proposal_buy.checked_sub(candidate.order_buy)?
    } else {
        // Buy order: user wants buy_amount, offers at most sell_amount.
        // Surplus = how much less sellToken the proposal consumes.
        candidate.order_sell.checked_sub(candidate.proposal_sell)?
    };

    // Convert surplus from token units to native-token (wei) units.
    let surplus_eth = surplus
        .checked_mul(native_price)?
        .checked_div(Unit::ETHER.wei())?;

    surplus_eth.checked_sub(candidate.gas_cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_gas_adds_the_buffer() {
        assert_eq!(effective_gas(200_000), 230_000);
    }

    #[test]
    fn sell_order_positive_surplus() {
        // Surplus token (buy token) is worth 0.5 ETH per 10^18 atoms.
        let score = score_proposal(
            &Candidate {
                order_sell: U256::from(1_000u64),
                order_buy: U256::from(900u64),
                proposal_sell: U256::from(1_000u64),
                proposal_buy: U256::from(950u64),
                is_sell_order: true,
                gas_cost: U256::ZERO,
            },
            SurplusPrice(Unit::ETHER.wei() / U256::from(2)),
        );
        // surplus = 950 - 900 = 50
        // surplus_eth = 50 * 0.5e18 / 1e18 = 25
        assert_eq!(score, Some(U256::from(25u64)));
    }

    #[test]
    fn buy_order_positive_surplus() {
        let score = score_proposal(
            &Candidate {
                order_sell: U256::from(1_000u64),
                order_buy: U256::from(900u64),
                proposal_sell: U256::from(950u64),
                proposal_buy: U256::from(900u64),
                is_sell_order: false,
                gas_cost: U256::ZERO,
            },
            SurplusPrice(Unit::ETHER.wei() / U256::from(2)),
        );
        // surplus = 1000 - 950 = 50
        // surplus_eth = 50 * 0.5e18 / 1e18 = 25
        assert_eq!(score, Some(U256::from(25u64)));
    }

    #[test]
    fn proposal_below_minimum_returns_none() {
        let score = score_proposal(
            &Candidate {
                order_sell: U256::from(1_000u64),
                order_buy: U256::from(900u64),
                proposal_sell: U256::from(1_000u64),
                proposal_buy: U256::from(800u64), // below order's buy minimum
                is_sell_order: true,
                gas_cost: U256::ZERO,
            },
            SurplusPrice(Unit::ETHER.wei()),
        );
        assert_eq!(score, None);
    }

    #[test]
    fn gas_exceeds_surplus_returns_none() {
        // Native price = 1:1 so surplus_eth equals surplus in token units.
        let score = score_proposal(
            &Candidate {
                order_sell: U256::from(1_000u64),
                order_buy: U256::from(900u64),
                proposal_sell: U256::from(1_000u64),
                proposal_buy: U256::from(910u64), // surplus = 10
                is_sell_order: true,
                gas_cost: U256::from(20u64), // gas = 20 > surplus_eth (10)
            },
            SurplusPrice(Unit::ETHER.wei()),
        );
        assert_eq!(score, None);
    }

    /// A surplus large enough to overflow the native-price conversion scores
    /// nothing rather than wrapping. The release profile has no overflow
    /// checks, so a wrapped product would rank an absurd proposal first.
    #[test]
    fn surplus_that_overflows_the_conversion_returns_none() {
        let score = score_proposal(
            &Candidate {
                order_sell: U256::ZERO,
                order_buy: U256::ZERO,
                proposal_sell: U256::ZERO,
                proposal_buy: U256::MAX,
                is_sell_order: true,
                gas_cost: U256::ZERO,
            },
            SurplusPrice(U256::from(2)),
        );
        assert_eq!(score, None);
    }

    #[test]
    fn zero_surplus_minus_zero_gas() {
        let score = score_proposal(
            &Candidate {
                order_sell: U256::from(1_000u64),
                order_buy: U256::from(900u64),
                proposal_sell: U256::from(1_000u64),
                proposal_buy: U256::from(900u64), // exactly at minimum
                is_sell_order: true,
                gas_cost: U256::ZERO,
            },
            SurplusPrice(Unit::ETHER.wei()),
        );
        assert_eq!(score, Some(U256::ZERO));
    }
}
