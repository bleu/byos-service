//! The **gas cut**: the fee BYOS declares on every fulfillment so the
//! settlement keeps back what submitting it costs (ADR-0002).
//!
//! Nothing reimburses gas. The cut is a price wedge, not a transfer: we declare
//! it as our solver fee and route the full sell amount anyway, so the
//! settlement pulls everything in, pays the user out at prices that leave the
//! cut behind, and the gap sits in the `GPv2Settlement` buffers until CoW's
//! weekly accounting values it in native token (byos-contracts ADR-0003). There
//! is nothing to transfer and no contract of our own to hold it.
//!
//! We cannot take the cut the way baseline does, by routing less than the user
//! sold: the sub-solver signed a route for the full `proposal.sell_amount` and
//! the Trampoline enforces delivery against the signed `buyAmount`, so the
//! input cannot be resized. Declaring the fee and routing the full amount is
//! the same wedge by different arithmetic, and it keeps `encode_settle`'s price
//! vector untouched — the transaction we simulated stays the transaction we
//! bid. It also books the cut as a declared solver fee in CoW's accounting
//! rather than as slippage.
//!
//! Sizing is exactly the gas estimate: always on, no multiplier, no config
//! knob. Padding it is never free — a bigger cut lowers our score, which lowers
//! CIP-85 consistency rewards, and those are allocated from a shared bucket by
//! how close our bids sit to the winner, so we do not recapture what we add to
//! it. Revenue margin above gas recovery is COW-1238.
//!
//! Known imprecision, recorded rather than fixed: the cut is sized from the
//! auction's native price (the heuristic the CoW team intends, occasionally
//! bogus), while the payout converts at an average observed over roughly an
//! hour around the trade. Padding does not close that gap; CoW's weekly
//! dashboard of gas paid against gas collected per solver is better feedback
//! than anything computed here.

use alloy::primitives::{U256, utils::Unit};

/// A proposal's gas cut, sized against one order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cut {
    /// The fee to declare on the fulfillment, in sell-token atoms.
    pub fee: U256,
    /// The fulfillment's `executed_amount`. The driver requires
    /// `executed + fee == order.target()` for sell orders and leaves the fee
    /// out of that check for buy orders
    /// (`driver/src/domain/competition/solution/trade.rs:147`).
    pub executed_amount: U256,
}

pub struct CutInput {
    pub order_sell: U256,
    pub order_buy: U256,
    pub proposal_sell: U256,
    pub proposal_buy: U256,
    pub is_sell_order: bool,
    /// Gas cost in wei
    /// ([`effective_gas`](super::scoring::effective_gas)` ×
    /// effective_gas_price`).
    pub gas_cost: U256,
    /// Auction reference price for the *sell* token: how much wei buys 10^18
    /// atoms of it. The cut is denominated in the sell token, which for a sell
    /// order is not the token the surplus is in.
    pub sell_token_price: U256,
}

/// Size the gas cut for one proposal and shape the fulfillment amounts around
/// it.
///
/// `None` when the cut cannot be sized (the auction gives the sell token no
/// price) or when taking it would push the user past the limit they signed.
/// That limit check is ours to make because the price is ours; it needs no fee
/// policies, which the auction only ever delivers per-`/solve` anyway.
pub fn gas_cut(input: &CutInput) -> Option<Cut> {
    if input.sell_token_price.is_zero() {
        return None;
    }
    // Round up: a cut an atom short of the gas bill is a loss, an atom over
    // costs the user a rounding error.
    let fee = input
        .gas_cost
        .checked_mul(Unit::ETHER.wei())?
        .div_ceil(input.sell_token_price);

    // Both branches reproduce what the driver will encode. It builds per-trade
    // clearing prices as `{sell: buy_amount(), buy: sell_amount()}` and GPv2
    // pays out `order.sellAmount × sell ÷ buy` for a fill-or-kill order, so the
    // amounts below are the ones the settlement's limit check will see
    // (`driver/src/domain/competition/solution/trade.rs:219-249`).
    let executed_amount = if input.is_sell_order {
        let executed = input.order_sell.checked_sub(fee)?;
        // We route the full sell amount but declare only `executed` of it, so
        // the user receives the route's output scaled by the same ratio:
        // `R × (S − f) / S`. Below the signed buy amount the settlement reverts,
        // so drop the proposal here. The chain rounds this up and we round it
        // down, which leaves us strictly on the safe side of the check.
        let received = input
            .proposal_buy
            .checked_mul(executed)?
            .checked_div(input.order_sell)?;
        if received < input.order_buy {
            return None;
        }
        executed
    } else {
        // Buy orders: the route's input plus the cut is what leaves the user's
        // wallet, and it cannot exceed the sell amount they signed.
        if input.proposal_sell.checked_add(fee)? > input.order_sell {
            return None;
        }
        input.order_buy
    };

    Some(Cut {
        fee,
        executed_amount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sell token worth 0.5 wei per atom, so a 100-wei gas bill costs the user
    /// 200 atoms of it.
    #[test]
    fn sell_order_cut_is_the_gas_cost_in_sell_token_atoms() {
        let cut = gas_cut(&CutInput {
            order_sell: U256::from(1_000_000u64),
            order_buy: U256::from(900_000u64),
            proposal_sell: U256::from(1_000_000u64),
            proposal_buy: U256::from(1_000_000u64),
            is_sell_order: true,
            gas_cost: U256::from(100u64),
            sell_token_price: Unit::ETHER.wei() / U256::from(2),
        });

        assert_eq!(
            cut,
            Some(Cut {
                fee: U256::from(200u64),
                executed_amount: U256::from(999_800u64),
            }),
        );
    }

    /// The 200-atom cut on a 1_000_000 sell shaves 0.02% off what the route
    /// delivers: 900_000 buy tokens become 899_820, under the 899_900 the user
    /// signed for. Bidding it would revert the settlement's limit check.
    #[test]
    fn sell_order_cut_that_breaches_the_signed_limit_is_skipped() {
        let cut = gas_cut(&CutInput {
            order_sell: U256::from(1_000_000u64),
            order_buy: U256::from(899_900u64),
            proposal_sell: U256::from(1_000_000u64),
            proposal_buy: U256::from(900_000u64),
            is_sell_order: true,
            gas_cost: U256::from(100u64),
            sell_token_price: Unit::ETHER.wei() / U256::from(2),
        });

        assert_eq!(cut, None);
    }

    /// Landing the user exactly on their signed buy amount still settles — the
    /// on-chain check is "at least", so the boundary belongs on the keep side.
    #[test]
    fn sell_order_cut_that_lands_exactly_on_the_limit_is_kept() {
        let cut = gas_cut(&CutInput {
            order_sell: U256::from(1_000_000u64),
            order_buy: U256::from(999_800u64),
            proposal_sell: U256::from(1_000_000u64),
            proposal_buy: U256::from(1_000_000u64),
            is_sell_order: true,
            gas_cost: U256::from(100u64),
            sell_token_price: Unit::ETHER.wei() / U256::from(2),
        });

        assert_eq!(cut.map(|c| c.executed_amount), Some(U256::from(999_800u64)));
    }

    /// A buy order's execution is the buy amount the user signed for, and the
    /// driver leaves the fee out of its `executed + fee == target` check
    /// (`driver/src/domain/competition/solution/trade.rs:154`), so the cut does
    /// not move it.
    #[test]
    fn buy_order_executes_the_signed_buy_amount() {
        let cut = gas_cut(&CutInput {
            order_sell: U256::from(1_000_000u64),
            order_buy: U256::from(900_000u64),
            proposal_sell: U256::from(950_000u64),
            proposal_buy: U256::from(900_000u64),
            is_sell_order: false,
            gas_cost: U256::from(100u64),
            sell_token_price: Unit::ETHER.wei() / U256::from(2),
        });

        assert_eq!(
            cut,
            Some(Cut {
                fee: U256::from(200u64),
                executed_amount: U256::from(900_000u64),
            }),
        );
    }

    /// On a buy order the user pays the route's input plus our cut. A route
    /// that already spends 999_900 of the signed 1_000_000 leaves no room for a
    /// 200-atom cut.
    #[test]
    fn buy_order_cut_over_the_signed_sell_amount_is_skipped() {
        let cut = gas_cut(&CutInput {
            order_sell: U256::from(1_000_000u64),
            order_buy: U256::from(900_000u64),
            proposal_sell: U256::from(999_900u64),
            proposal_buy: U256::from(900_000u64),
            is_sell_order: false,
            gas_cost: U256::from(100u64),
            sell_token_price: Unit::ETHER.wei() / U256::from(2),
        });

        assert_eq!(cut, None);
    }

    /// Fractions of an atom round our way: 3 wei of gas at 2 wei per atom is
    /// 1.5 atoms, and we take 2.
    #[test]
    fn a_cut_that_does_not_divide_evenly_rounds_up() {
        let cut = gas_cut(&CutInput {
            order_sell: U256::from(1_000_000u64),
            order_buy: U256::from(900_000u64),
            proposal_sell: U256::from(1_000_000u64),
            proposal_buy: U256::from(1_000_000u64),
            is_sell_order: true,
            gas_cost: U256::from(3u64),
            sell_token_price: Unit::ETHER.wei() * U256::from(2),
        });

        assert_eq!(cut.map(|c| c.fee), Some(U256::from(2u64)));
    }

    /// The auction prices tokens it knows; an unpriced sell token comes through
    /// as zero. We cannot size a cut against it, and bidding without one would
    /// hand the driver a `fee: None` it rejects, so skip the proposal.
    #[test]
    fn an_unpriced_sell_token_cannot_be_cut() {
        let cut = gas_cut(&CutInput {
            order_sell: U256::from(1_000_000u64),
            order_buy: U256::from(900_000u64),
            proposal_sell: U256::from(1_000_000u64),
            proposal_buy: U256::from(1_000_000u64),
            is_sell_order: true,
            gas_cost: U256::from(100u64),
            sell_token_price: U256::ZERO,
        });

        assert_eq!(cut, None);
    }

    /// A price low enough to make the scaling overflow is a bad price, not an
    /// expensive trade.
    #[test]
    fn a_cut_that_overflows_the_scaling_is_skipped() {
        let cut = gas_cut(&CutInput {
            order_sell: U256::from(1_000_000u64),
            order_buy: U256::from(900_000u64),
            proposal_sell: U256::from(1_000_000u64),
            proposal_buy: U256::from(1_000_000u64),
            is_sell_order: true,
            gas_cost: U256::MAX,
            sell_token_price: U256::from(1u64),
        });

        assert_eq!(cut, None);
    }
}
