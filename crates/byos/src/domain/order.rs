//! The orderbook order a proposal settles, plus the validation envelope
//! (ADR-0012): which orders the simulation supports at all.

use {
    super::{proposal::Proposal, validator::RejectionReason},
    byos_common::{
        contracts::GPv2InteractionData,
        settlement::{CowOrder, OrderKind},
    },
};

/// An order fetched from the orderbook, with the facts the envelope check
/// and simulation need. Immutable once fetched (orders never change;
/// off-chain cancellation is accepted staleness, see ADR-0012).
#[derive(Clone, Debug)]
pub struct OrderRecord {
    pub order: CowOrder,
    /// Pre-hook interactions from the orderbook's `interactions.pre` field,
    /// already trampoline-wrapped by the orderbook. Included in simulation
    /// for accurate gas estimates; NOT returned by `/solve` (the driver
    /// appends hooks itself).
    pub pre_interactions: Vec<GPv2InteractionData>,
    /// Post-hook interactions from the orderbook's `interactions.post` field.
    pub post_interactions: Vec<GPv2InteractionData>,
    /// True when `fullAppData` declares `metadata.bridging`.
    pub has_bridging: bool,
    /// True when both balance locations are plain `erc20`.
    pub erc20_balances: bool,
}

impl OrderRecord {
    /// Checks the proposal/order pair against the simulation envelope.
    /// `Err` carries the rejection reason to store on the proposal.
    pub fn check_envelope(&self, proposal: &Proposal) -> Result<(), RejectionReason> {
        if self.has_bridging || !self.erc20_balances {
            return Err(RejectionReason::UnsupportedOrder);
        }
        if self.order.partially_fillable {
            self.check_partial_fill(proposal)
        } else {
            self.check_fill_or_kill(proposal)
        }
    }

    /// Fill-or-kill: the proposal must quote exactly the order's target
    /// amount. A mismatch would simulate a different trade than the one the
    /// driver settles.
    fn check_fill_or_kill(&self, proposal: &Proposal) -> Result<(), RejectionReason> {
        let amounts_match = match self.order.kind {
            OrderKind::Sell => proposal.sell_amount == self.order.sell_amount,
            OrderKind::Buy => proposal.buy_amount == self.order.buy_amount,
        };
        if !amounts_match {
            return Err(RejectionReason::AmountMismatch);
        }
        Ok(())
    }

    /// Partially fillable: the proposal may fill any fraction of the order,
    /// but the fill must be non-zero, must not exceed the signed order
    /// amount, and must respect the order's limit price.
    fn check_partial_fill(&self, proposal: &Proposal) -> Result<(), RejectionReason> {
        match self.order.kind {
            OrderKind::Sell => {
                if proposal.sell_amount.is_zero() || proposal.sell_amount > self.order.sell_amount {
                    return Err(RejectionReason::AmountMismatch);
                }
                // Limit price: proposal_buy / proposal_sell >= order_buy / order_sell
                // Cross-multiply to avoid division:
                //   proposal_buy * order_sell >= proposal_sell * order_buy
                let lhs = proposal.buy_amount.checked_mul(self.order.sell_amount);
                let rhs = proposal.sell_amount.checked_mul(self.order.buy_amount);
                match (lhs, rhs) {
                    (Some(l), Some(r)) if l >= r => Ok(()),
                    _ => Err(RejectionReason::AmountMismatch),
                }
            }
            OrderKind::Buy => {
                if proposal.buy_amount.is_zero()
                    || proposal.sell_amount.is_zero()
                    || proposal.buy_amount > self.order.buy_amount
                {
                    return Err(RejectionReason::AmountMismatch);
                }
                // Limit price: proposal_sell / proposal_buy <= order_sell / order_buy
                // Cross-multiply:
                //   proposal_sell * order_buy <= proposal_buy * order_sell
                let lhs = proposal.sell_amount.checked_mul(self.order.buy_amount);
                let rhs = proposal.buy_amount.checked_mul(self.order.sell_amount);
                match (lhs, rhs) {
                    (Some(l), Some(r)) if l <= r => Ok(()),
                    _ => Err(RejectionReason::AmountMismatch),
                }
            }
        }
    }
}

/// Test fixture: an in-envelope fill-or-kill sell order matching
/// [`super::proposal::test_proposal`]: same sell amount (the envelope
/// requirement for sell orders), buy limit below the proposal's 990_000 so
/// the pair carries positive surplus.
#[cfg(test)]
pub(crate) fn test_order_record() -> OrderRecord {
    use {
        alloy::primitives::{Address, Bytes, U256, address},
        byos_common::settlement::SigningScheme,
    };
    OrderRecord {
        order: CowOrder {
            sell_token: address!("00000000000000000000000000000000000000aa"),
            buy_token: address!("00000000000000000000000000000000000000bb"),
            receiver: Address::ZERO,
            sell_amount: U256::from(1_000_000_u64),
            buy_amount: U256::from(980_000_u64),
            valid_to: u32::MAX,
            app_data: Default::default(),
            fee_amount: U256::ZERO,
            kind: OrderKind::Sell,
            partially_fillable: false,
            signing_scheme: SigningScheme::Eip712,
            signature: Bytes::from(vec![0u8; 65]),
        },
        pre_interactions: vec![],
        post_interactions: vec![],
        has_bridging: false,
        erc20_balances: true,
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::domain::proposal::{ProposalStatus, test_proposal},
        alloy::primitives::{Address, U256},
    };

    fn sample_order() -> OrderRecord {
        test_order_record()
    }

    /// A proposal inside `sample_order`'s envelope.
    fn matching_proposal() -> Proposal {
        // test_proposal sells 1_000_000, matching sample_order's sell amount
        // (the envelope requirement for a sell order).
        test_proposal(
            crate::domain::proposal::OrderUid([0u8; 56]),
            Address::ZERO,
            ProposalStatus::Submitted,
        )
    }

    // -- partial fill: sell orders --

    #[test]
    fn partial_fill_sell_order_within_limit_passes() {
        let mut record = sample_order();
        record.order.partially_fillable = true;
        // Proposal fills half the order at a better-than-limit price.
        let mut proposal = matching_proposal();
        proposal.sell_amount = U256::from(500_000_u64);
        proposal.buy_amount = U256::from(495_000_u64); // > 490_000 scaled limit
        assert_eq!(record.check_envelope(&proposal), Ok(()));
    }

    #[test]
    fn partial_fill_sell_order_at_exact_limit_passes() {
        let mut record = sample_order();
        record.order.partially_fillable = true;
        // order limit ratio = 980_000 / 1_000_000 = 0.98
        // Half fill at exactly the limit: 500_000 sell, 490_000 buy
        let mut proposal = matching_proposal();
        proposal.sell_amount = U256::from(500_000_u64);
        proposal.buy_amount = U256::from(490_000_u64);
        assert_eq!(record.check_envelope(&proposal), Ok(()));
    }

    #[test]
    fn partial_fill_sell_order_below_limit_price_rejected() {
        let mut record = sample_order();
        record.order.partially_fillable = true;
        let mut proposal = matching_proposal();
        proposal.sell_amount = U256::from(500_000_u64);
        proposal.buy_amount = U256::from(489_999_u64); // below 490_000 scaled limit
        assert_eq!(
            record.check_envelope(&proposal),
            Err(RejectionReason::AmountMismatch),
        );
    }

    #[test]
    fn partial_fill_sell_order_exceeding_order_amount_rejected() {
        let mut record = sample_order();
        record.order.partially_fillable = true;
        let mut proposal = matching_proposal();
        proposal.sell_amount = U256::from(1_000_001_u64);
        assert_eq!(
            record.check_envelope(&proposal),
            Err(RejectionReason::AmountMismatch),
        );
    }

    #[test]
    fn partial_fill_sell_order_zero_amount_rejected() {
        let mut record = sample_order();
        record.order.partially_fillable = true;
        let mut proposal = matching_proposal();
        proposal.sell_amount = U256::ZERO;
        assert_eq!(
            record.check_envelope(&proposal),
            Err(RejectionReason::AmountMismatch),
        );
    }

    #[test]
    fn partial_fill_buy_order_zero_sell_amount_rejected() {
        let mut record = sample_order();
        record.order.kind = OrderKind::Buy;
        record.order.partially_fillable = true;
        let mut proposal = matching_proposal();
        proposal.buy_amount = U256::from(490_000_u64);
        proposal.sell_amount = U256::ZERO;
        assert_eq!(
            record.check_envelope(&proposal),
            Err(RejectionReason::AmountMismatch),
        );
    }

    // -- partial fill: buy orders --

    #[test]
    fn partial_fill_buy_order_within_limit_passes() {
        let mut record = sample_order();
        record.order.kind = OrderKind::Buy;
        record.order.partially_fillable = true;
        // order: sell 1_000_000, buy 980_000
        // proposal fills half: buy 490_000, sell 490_000 (< 500_000 scaled limit)
        let mut proposal = matching_proposal();
        proposal.buy_amount = U256::from(490_000_u64);
        proposal.sell_amount = U256::from(490_000_u64);
        assert_eq!(record.check_envelope(&proposal), Ok(()));
    }

    #[test]
    fn partial_fill_buy_order_exceeding_order_amount_rejected() {
        let mut record = sample_order();
        record.order.kind = OrderKind::Buy;
        record.order.partially_fillable = true;
        let mut proposal = matching_proposal();
        proposal.buy_amount = U256::from(980_001_u64);
        assert_eq!(
            record.check_envelope(&proposal),
            Err(RejectionReason::AmountMismatch),
        );
    }

    // -- partial fill: full-amount fill (degenerate case) --

    #[test]
    fn partial_fill_at_full_amount_passes() {
        let mut record = sample_order();
        record.order.partially_fillable = true;
        // Fill the entire order — should behave like fill-or-kill.
        assert_eq!(record.check_envelope(&matching_proposal()), Ok(()));
    }

    #[test]
    fn non_erc20_balance_order_is_rejected() {
        let mut record = sample_order();
        record.erc20_balances = false;

        assert_eq!(
            record.check_envelope(&matching_proposal()),
            Err(RejectionReason::UnsupportedOrder),
        );
    }

    #[test]
    fn sell_order_with_mismatched_sell_amount_is_rejected() {
        let mut record = sample_order();
        record.order.sell_amount = U256::from(999_999_u64);

        assert_eq!(
            record.check_envelope(&matching_proposal()),
            Err(RejectionReason::AmountMismatch),
        );
    }

    #[test]
    fn buy_order_with_mismatched_buy_amount_is_rejected() {
        let mut record = sample_order();
        record.order.kind = OrderKind::Buy;
        record.order.buy_amount = U256::from(1_u64);
        // Sell amount no longer needs to match for a buy order.
        record.order.sell_amount = U256::from(42_u64);

        assert_eq!(
            record.check_envelope(&matching_proposal()),
            Err(RejectionReason::AmountMismatch),
        );
    }

    #[test]
    fn matching_fill_or_kill_order_passes() {
        assert_eq!(sample_order().check_envelope(&matching_proposal()), Ok(()));
    }

    #[test]
    fn bridging_order_is_rejected() {
        let mut record = sample_order();
        record.has_bridging = true;

        assert_eq!(
            record.check_envelope(&matching_proposal()),
            Err(RejectionReason::UnsupportedOrder),
        );
    }

    #[test]
    fn hooked_order_passes_envelope() {
        let mut record = sample_order();
        record.pre_interactions = vec![byos_common::contracts::GPv2InteractionData {
            target: alloy::primitives::Address::ZERO,
            value: alloy::primitives::U256::ZERO,
            callData: alloy::primitives::Bytes::new(),
        }];

        assert_eq!(record.check_envelope(&matching_proposal()), Ok(()));
    }
}
