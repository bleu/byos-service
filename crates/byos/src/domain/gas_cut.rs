//! The gas cut: what BYOS keeps back to cover submitting a settlement.
//!
//! Sized at exactly the gas estimate and declared as the solution's solver fee,
//! while the route still carries the full sell amount. Rationale, and why it is
//! not a percentage of `sellAmount` or a shaded price, in ADR-0002 §Gas cut.

use {
    super::scoring::Candidate,
    alloy::primitives::{U256, utils::Unit},
};

/// The auction's reference price for the order's sell token: wei per 10^18
/// atoms. Its own type because on a sell order the surplus lands in the buy
/// token, so [`SurplusPrice`](super::scoring::SurplusPrice) is a different
/// number for the same trade and the two are one argument apart.
#[derive(Clone, Copy, Debug)]
pub struct SellTokenPrice(pub U256);

/// A proposal's gas cut, sized against one order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GasCut {
    /// The cut itself, in sell-token atoms. Goes on the wire as the
    /// fulfillment's `fee`, which is unrelated to the order's signed
    /// `feeAmount` (zero on every live order).
    pub amount: U256,
    /// The fulfillment's `executed_amount`. The driver requires
    /// `executed + fee == order.target()` for sell orders and leaves the cut
    /// out of that check for buy orders.
    pub executed_amount: U256,
}

/// Size the cut for one proposal and shape the fulfillment amounts around it.
///
/// `None` when the sell token has no price, or when taking the cut would push
/// the user past the limit they signed.
pub fn size(candidate: &Candidate, sell_token_price: SellTokenPrice) -> Option<GasCut> {
    let SellTokenPrice(sell_token_price) = sell_token_price;
    if sell_token_price.is_zero() {
        return None;
    }
    // Round up: a cut an atom short of the gas bill is a loss, an atom over
    // costs the user a rounding error.
    let amount = candidate
        .gas_cost
        .checked_mul(Unit::ETHER.wei())?
        .div_ceil(sell_token_price);

    // The executed amount is the order's, because that is what the driver
    // checks it against. What the chain then derives from it runs through the
    // clearing prices, which are the proposal's amounts — so the sell-side
    // scaling divides by those.
    let executed_amount = if candidate.is_sell_order {
        let executed = candidate.order_sell.checked_sub(amount)?;
        // Declaring only `executed` of the full sell amount scales what the user
        // receives by the same ratio. The chain rounds that up where we round
        // down, so this errs toward not bidding.
        let received = candidate
            .proposal_buy
            .checked_mul(executed)?
            .checked_div(candidate.proposal_sell)?;
        if received < candidate.order_buy {
            return None;
        }
        executed
    } else {
        // The route's input plus the cut is what leaves the user's wallet.
        // Exact only because the envelope pins `proposal_buy` to `order_buy`;
        // otherwise the chain would scale this by the ratio between them.
        if candidate.proposal_sell.checked_add(amount)? > candidate.order_sell {
            return None;
        }
        candidate.order_buy
    };

    Some(GasCut {
        amount,
        executed_amount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sell token worth 0.5 wei per atom, so a 100-wei gas bill costs 200
    /// atoms.
    fn sell_order(order_buy: u64, proposal_buy: u64, gas_cost: u64) -> Option<GasCut> {
        size(
            &Candidate {
                order_sell: U256::from(1_000_000u64),
                order_buy: U256::from(order_buy),
                proposal_sell: U256::from(1_000_000u64),
                proposal_buy: U256::from(proposal_buy),
                is_sell_order: true,
                gas_cost: U256::from(gas_cost),
            },
            SellTokenPrice(Unit::ETHER.wei() / U256::from(2)),
        )
    }

    #[test]
    fn sell_order_cut_is_the_gas_cost_in_sell_token_atoms() {
        assert_eq!(
            sell_order(900_000, 1_000_000, 100),
            Some(GasCut {
                amount: U256::from(200u64),
                executed_amount: U256::from(999_800u64),
            }),
        );
    }

    /// The cut shaves 0.02% off what the route delivers: 900_000 buy tokens
    /// become 899_820, under the 899_900 signed for.
    #[test]
    fn sell_order_cut_that_breaches_the_signed_limit_is_skipped() {
        assert_eq!(sell_order(899_900, 900_000, 100), None);
    }

    /// The on-chain check is "at least", so the boundary keeps the proposal.
    #[test]
    fn sell_order_cut_that_lands_exactly_on_the_limit_is_kept() {
        assert_eq!(
            sell_order(999_800, 1_000_000, 100).map(|c| c.executed_amount),
            Some(U256::from(999_800u64)),
        );
    }

    /// 3 wei of gas at 2 wei per atom is 1.5 atoms, and we take 2.
    #[test]
    fn a_cut_that_does_not_divide_evenly_rounds_up() {
        let cut = size(
            &Candidate {
                order_sell: U256::from(1_000_000u64),
                order_buy: U256::from(900_000u64),
                proposal_sell: U256::from(1_000_000u64),
                proposal_buy: U256::from(1_000_000u64),
                is_sell_order: true,
                gas_cost: U256::from(3u64),
            },
            SellTokenPrice(Unit::ETHER.wei() * U256::from(2)),
        );

        assert_eq!(cut.map(|c| c.amount), Some(U256::from(2u64)));
    }

    /// An unpriced token arrives as zero. Bidding without a cut would hand the
    /// driver a `fee: None` it rejects, so skip the proposal.
    #[test]
    fn an_unpriced_sell_token_cannot_be_cut() {
        let cut = size(
            &Candidate {
                order_sell: U256::from(1_000_000u64),
                order_buy: U256::from(900_000u64),
                proposal_sell: U256::from(1_000_000u64),
                proposal_buy: U256::from(1_000_000u64),
                is_sell_order: true,
                gas_cost: U256::from(100u64),
            },
            SellTokenPrice(U256::ZERO),
        );

        assert_eq!(cut, None);
    }

    /// A price low enough to overflow the scaling is a bad price, not an
    /// expensive trade.
    #[test]
    fn a_cut_that_overflows_the_scaling_is_skipped() {
        let cut = size(
            &Candidate {
                order_sell: U256::from(1_000_000u64),
                order_buy: U256::from(900_000u64),
                proposal_sell: U256::from(1_000_000u64),
                proposal_buy: U256::from(1_000_000u64),
                is_sell_order: true,
                gas_cost: U256::MAX,
            },
            SellTokenPrice(U256::from(1u64)),
        );

        assert_eq!(cut, None);
    }

    /// The driver leaves the cut out of its `executed + fee == target` check
    /// for buy orders, so it does not move the execution.
    #[test]
    fn buy_order_executes_the_signed_buy_amount() {
        let cut = size(
            &Candidate {
                order_sell: U256::from(1_000_000u64),
                order_buy: U256::from(900_000u64),
                proposal_sell: U256::from(950_000u64),
                proposal_buy: U256::from(900_000u64),
                is_sell_order: false,
                gas_cost: U256::from(100u64),
            },
            SellTokenPrice(Unit::ETHER.wei() / U256::from(2)),
        );

        assert_eq!(
            cut,
            Some(GasCut {
                amount: U256::from(200u64),
                executed_amount: U256::from(900_000u64),
            }),
        );
    }

    /// A route already spending 999_900 of the signed 1_000_000 leaves no room
    /// for a 200-atom cut on top.
    #[test]
    fn buy_order_cut_over_the_signed_sell_amount_is_skipped() {
        let cut = size(
            &Candidate {
                order_sell: U256::from(1_000_000u64),
                order_buy: U256::from(900_000u64),
                proposal_sell: U256::from(999_900u64),
                proposal_buy: U256::from(900_000u64),
                is_sell_order: false,
                gas_cost: U256::from(100u64),
            },
            SellTokenPrice(Unit::ETHER.wei() / U256::from(2)),
        );

        assert_eq!(cut, None);
    }
}
