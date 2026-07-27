//! The orderbook order a proposal settles, plus the validation envelope
//! (ADR-0012): which orders the simulation supports at all.

use {
    super::{proposal::Proposal, validator::RejectionReason},
    byos_common::settlement::{CowOrder, OrderKind},
};

/// An order fetched from the orderbook, with the fullAppData-derived facts
/// the envelope check needs. Immutable once fetched (orders never change;
/// off-chain cancellation is accepted staleness, see ADR-0012).
#[derive(Clone, Debug)]
pub struct OrderRecord {
    pub order: CowOrder,
    /// True when fullAppData declares hooks (`metadata.hooks`, or
    /// `metadata.bridging` which implies them).
    pub has_hooks: bool,
    /// True when both balance locations are plain `erc20`.
    pub erc20_balances: bool,
}

impl OrderRecord {
    /// Checks the proposal/order pair against the simulation envelope.
    /// `Err` carries the rejection reason to store on the proposal.
    pub fn check_envelope(&self, proposal: &Proposal) -> Result<(), RejectionReason> {
        if self.has_hooks || self.order.partially_fillable || !self.erc20_balances {
            return Err(RejectionReason::UnsupportedOrder);
        }
        // Fill-or-kill executes the order amount in full; a proposal quoting
        // a different amount would simulate a different trade than the one
        // the driver settles.
        let amounts_match = match self.order.kind {
            OrderKind::Sell => proposal.sell_amount == self.order.sell_amount,
            OrderKind::Buy => proposal.buy_amount == self.order.buy_amount,
        };
        if !amounts_match {
            return Err(RejectionReason::AmountMismatch);
        }
        Ok(())
    }
}

/// Test fixture: an in-envelope fill-or-kill sell order whose amounts match
/// [`super::proposal::test_proposal`].
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
            buy_amount: U256::from(990_000_u64),
            valid_to: u32::MAX,
            app_data: Default::default(),
            fee_amount: U256::ZERO,
            kind: OrderKind::Sell,
            partially_fillable: false,
            signing_scheme: SigningScheme::Eip712,
            signature: Bytes::from(vec![0u8; 65]),
        },
        has_hooks: false,
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

    /// A proposal whose amounts match `sample_order` exactly.
    fn matching_proposal() -> Proposal {
        // test_proposal uses sell=1_000_000, buy=990_000 — same as
        // sample_order.
        test_proposal(
            crate::domain::proposal::OrderUid([0u8; 56]),
            Address::ZERO,
            ProposalStatus::Submitted,
        )
    }

    #[test]
    fn partially_fillable_order_is_rejected() {
        let mut record = sample_order();
        record.order.partially_fillable = true;

        assert_eq!(
            record.check_envelope(&matching_proposal()),
            Err(RejectionReason::UnsupportedOrder),
        );
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
    fn hooked_order_is_rejected() {
        let mut record = sample_order();
        record.has_hooks = true;

        assert_eq!(
            record.check_envelope(&matching_proposal()),
            Err(RejectionReason::UnsupportedOrder),
        );
    }
}
