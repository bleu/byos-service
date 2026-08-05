//! Full `settle()` calldata encoding for proposal simulation (ADR-0012).
//!
//! Builds the one-trade settlement the CoW driver would submit for a
//! proposal: the real order as the single trade, clearing prices taken from
//! the proposal amounts, and the two trampoline intra-interactions from
//! [`crate::trampoline::encode_trampoline_interactions`].

use {
    crate::contracts::{GPv2InteractionData, GPv2Settlement, GPv2TradeData, Interaction, Proposal},
    alloy::{
        primitives::{Address, B256, Bytes, U256},
        sol_types::SolCall,
    },
};

/// Order side, as reported by the orderbook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderKind {
    Sell,
    Buy,
}

/// How the order owner signed, as reported by the orderbook. Determines the
/// signing-scheme bits of the trade's `flags` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SigningScheme {
    Eip712,
    EthSign,
    Eip1271,
    PreSign,
}

/// The slice of an orderbook order that `settle()` encoding needs. Field
/// values must be passed through from `GET /api/v1/orders/{uid}` untouched —
/// the order signature covers them.
#[derive(Clone, Debug)]
pub struct CowOrder {
    pub sell_token: Address,
    pub buy_token: Address,
    /// Zero address means "same as owner" (GPv2 convention).
    pub receiver: Address,
    pub sell_amount: U256,
    pub buy_amount: U256,
    pub valid_to: u32,
    pub app_data: B256,
    pub fee_amount: U256,
    pub kind: OrderKind,
    pub partially_fillable: bool,
    pub signing_scheme: SigningScheme,
    pub signature: Bytes,
}

/// Encodes the full `settle()` calldata simulating this proposal: tokens
/// `[sell, buy]`, clearing prices `[proposal.buyAmount, proposal.sellAmount]`
/// (so the user is paid exactly the proposal's clearing amounts), the order
/// as a single trade, and the trampoline intra-interactions.
///
/// `pre_interactions` and `post_interactions` are spliced into
/// `interactions[0]` and `interactions[2]` respectively — used for order
/// hooks (`HooksTrampoline.execute`).
pub fn encode_settle(
    order: &CowOrder,
    proposal: &Proposal,
    trampoline: Address,
    route: &[Interaction],
    proposal_signature: &Bytes,
    pre_interactions: &[GPv2InteractionData],
    post_interactions: &[GPv2InteractionData],
) -> Bytes {
    let trade = GPv2TradeData {
        sellTokenIndex: U256::ZERO,
        buyTokenIndex: U256::ONE,
        receiver: order.receiver,
        sellAmount: order.sell_amount,
        buyAmount: order.buy_amount,
        validTo: order.valid_to,
        appData: order.app_data,
        feeAmount: order.fee_amount,
        flags: trade_flags(order),
        // Ignored by GPv2 for fill-or-kill orders; set to the order amount
        // the trade executes in full.
        executedAmount: match order.kind {
            OrderKind::Sell => order.sell_amount,
            OrderKind::Buy => order.buy_amount,
        },
        signature: order.signature.clone(),
    };

    let intra = crate::trampoline::encode_trampoline_interactions(
        trampoline,
        order.sell_token,
        proposal,
        route,
        order.buy_token,
        proposal_signature,
    )
    .map(|i| GPv2InteractionData {
        target: i.target,
        value: i.value,
        callData: i.callData,
    });

    GPv2Settlement::settleCall {
        tokens: vec![order.sell_token, order.buy_token],
        clearingPrices: vec![proposal.buyAmount, proposal.sellAmount],
        trades: vec![trade],
        interactions: [
            pre_interactions.to_vec(),
            intra.to_vec(),
            post_interactions.to_vec(),
        ],
    }
    .abi_encode()
    .into()
}

/// Encodes the trade's `flags` word per `GPv2Trade.extractOrder`: bit 0 order
/// kind, bit 1 partial fill, bits 2-4 balance locations (always erc20 — the
/// validation envelope rejects other flavors), bits 5-6 signing scheme.
fn trade_flags(order: &CowOrder) -> U256 {
    let mut flags = 0u64;
    if order.kind == OrderKind::Buy {
        flags |= 1;
    }
    if order.partially_fillable {
        flags |= 1 << 1;
    }
    flags |= match order.signing_scheme {
        SigningScheme::Eip712 => 0,
        SigningScheme::EthSign => 1 << 5,
        SigningScheme::Eip1271 => 2 << 5,
        SigningScheme::PreSign => 3 << 5,
    };
    U256::from(flags)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::contracts::Trampoline,
        alloy::primitives::{address, b256, hex, keccak256},
    };

    fn decode_settle(calldata: &Bytes) -> GPv2Settlement::settleCall {
        GPv2Settlement::settleCall::abi_decode(calldata).expect("should decode as settle()")
    }

    /// Decodes the `execute` call out of the second intra-interaction.
    fn decode_execute(calldata: &Bytes) -> Trampoline::executeCall {
        Trampoline::executeCall::abi_decode(&decode_settle(calldata).interactions[1][1].callData)
            .expect("second intra-interaction should decode as execute()")
    }

    /// Real mainnet order 0xb9403b4c... (fetched from the orderbook
    /// 2026-07-27) with pinned synthetic proposal inputs. The expected
    /// calldata in testdata/settle-calldata.hex is Solidity's
    /// `abi.encodeCall(IGPv2Settlement.settle, ...)` for the same inputs,
    /// generated by the `SettleFixture` script in the `byos-contracts`
    /// submodule. Regenerate it there when the pin moves (ADR-0014).
    fn fixture_order() -> CowOrder {
        CowOrder {
            sell_token: address!("B1F1ee126e9c96231Cc3d3fAD7C08b4cf873b1f1"),
            buy_token: address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            receiver: address!("d2e80D60aff5377587E49FF32c9bad639d6f68Bc"),
            sell_amount: U256::from(20_000_002_675_677_095_795_u128),
            buy_amount: U256::from(773_213_156_u64),
            valid_to: 1_785_170_912,
            app_data: b256!("06ebf0fd49ea441fbd174e445f37f792eb8ee8848c66c470f59d06a1c3e318a4"),
            fee_amount: U256::ZERO,
            kind: OrderKind::Sell,
            partially_fillable: false,
            signing_scheme: SigningScheme::Eip712,
            signature: hex!(
                "45bcd35b2abeeafca8cd2ea00bd662ab327e0ffd7cd38319eeff8432fd49409f6e56384a88dcdc050d92b389285c3cfd78c903f3a20f64641b9f907dbf9de8b71c"
            )
            .into(),
        }
    }

    fn fixture_proposal() -> Proposal {
        let order_uid = hex!(
            "b9403b4c8342c3567e5b1928398030f010730c0b1d83657248e4e4e47984d90bd2e80d60aff5377587e49ff32c9bad639d6f68bc6a678be0"
        );
        Proposal {
            orderUidHash: keccak256(order_uid),
            sellAmount: U256::from(20_000_002_675_677_095_795_u128),
            buyAmount: U256::from(773_213_156_u64),
            validUntil: U256::from(1_785_174_512_u64),
            nonce: U256::ZERO,
        }
    }

    #[test]
    fn settle_calldata_matches_solidity_encoding() {
        let route = vec![Interaction {
            target: address!("0000000000000000000000000000000000004444"),
            value: U256::ZERO,
            callData: hex!("abcd").into(),
        }];
        let proposal_signature = Bytes::from(vec![0x11u8; 65]);

        let calldata = encode_settle(
            &fixture_order(),
            &fixture_proposal(),
            address!("0000000000000000000000000000000000007777"),
            &route,
            &proposal_signature,
            &[],
            &[],
        );

        let expected = include_str!("../testdata/settle-calldata.hex").trim();
        assert_eq!(format!("0x{}", hex::encode(&calldata)), expected);
    }

    #[test]
    fn buy_order_flags_and_executed_amount() {
        let mut order = fixture_order();
        order.kind = OrderKind::Buy;
        order.signing_scheme = SigningScheme::Eip1271;

        let calldata = encode_settle(
            &order,
            &fixture_proposal(),
            address!("0000000000000000000000000000000000007777"),
            &[],
            &Bytes::from(vec![0x11u8; 65]),
            &[],
            &[],
        );

        let decoded = decode_settle(&calldata);
        let trade = &decoded.trades[0];
        // bit 0 = buy, bits 5-6 = eip1271 (0b10).
        assert_eq!(trade.flags, U256::from(1u64 | (2 << 5)));
        assert_eq!(trade.executedAmount, order.buy_amount);
    }

    /// GPv2's pseudo-token for orders buying native ETH. The Trampoline reads
    /// it to take its native-ETH branch: the balance snapshot, the sweep, and
    /// the floor delta all run in ETH rather than through an ERC-20 call.
    const BUY_ETH_ADDRESS: Address = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");

    #[test]
    fn native_eth_buy_order_passes_the_marker_through() {
        let mut order = fixture_order();
        order.buy_token = BUY_ETH_ADDRESS;
        let proposal = fixture_proposal();

        let calldata = encode_settle(
            &order,
            &proposal,
            address!("0000000000000000000000000000000000007777"),
            &[],
            &Bytes::from(vec![0x11u8; 65]),
            &[],
            &[],
        );

        let settle = decode_settle(&calldata);

        // The marker is the buy-side token GPv2 prices the trade against, and
        // the one the Trampoline branches on.
        assert_eq!(settle.tokens, vec![order.sell_token, BUY_ETH_ADDRESS]);
        assert_eq!(decode_execute(&calldata)._buyToken, BUY_ETH_ADDRESS);

        // The sell leg still moves a real ERC-20 into the instance.
        assert_eq!(settle.interactions[1][0].target, order.sell_token);

        // The user is paid the proposal's buy amount, as for any other order.
        assert_eq!(
            settle.clearingPrices,
            vec![proposal.buyAmount, proposal.sellAmount]
        );
    }

    #[test]
    fn same_token_order_still_pays_the_proposal_buy_amount() {
        // CoW carries sellToken == buyToken orders, submitted mainly to run
        // hooks, where sellAmount always exceeds buyAmount. The Trampoline
        // sweeps the shared token once.
        let mut order = fixture_order();
        order.buy_token = order.sell_token;
        order.buy_amount = U256::from(19_000_000_000_000_000_000_u128);
        let proposal = Proposal {
            buyAmount: order.buy_amount,
            ..fixture_proposal()
        };

        let calldata = encode_settle(
            &order,
            &proposal,
            address!("0000000000000000000000000000000000007777"),
            &[],
            &Bytes::from(vec![0x11u8; 65]),
            &[],
            &[],
        );

        let settle = decode_settle(&calldata);

        // Both trade legs name the same address, so execute takes its
        // sweep-once branch instead of transferring the token twice.
        let execute = decode_execute(&calldata);
        assert_eq!(execute._sellToken, execute._buyToken);
        assert_eq!(settle.tokens, vec![order.sell_token, order.sell_token]);

        // GPv2 prices a sell order as sellAmount * price[sell] / price[buy];
        // the cross-multiplied prices still pay out exactly the signed floor.
        let paid = order.sell_amount * settle.clearingPrices[0] / settle.clearingPrices[1];
        assert_eq!(paid, proposal.buyAmount);
    }

    /// Non-empty pre/post hooks land in `interactions[0]` and
    /// `interactions[2]`, alongside the two trampoline intra-interactions
    /// in `interactions[1]`.
    #[test]
    fn pre_and_post_hook_interactions_are_spliced_into_the_settlement() {
        let pre = vec![GPv2InteractionData {
            target: address!("000000000000000000000000000000000000aaaa"),
            value: U256::ZERO,
            callData: hex!("11111111").into(),
        }];
        let post = vec![
            GPv2InteractionData {
                target: address!("000000000000000000000000000000000000bbbb"),
                value: U256::ZERO,
                callData: hex!("22222222").into(),
            },
            GPv2InteractionData {
                target: address!("000000000000000000000000000000000000cccc"),
                value: U256::ZERO,
                callData: hex!("33333333").into(),
            },
        ];

        let calldata = encode_settle(
            &fixture_order(),
            &fixture_proposal(),
            address!("0000000000000000000000000000000000007777"),
            &[],
            &Bytes::from(vec![0x11u8; 65]),
            &pre,
            &post,
        );

        let settle = decode_settle(&calldata);

        // interactions[0] = pre-hooks
        assert_eq!(
            settle.interactions[0].len(),
            1,
            "one pre-hook interaction expected",
        );
        assert_eq!(settle.interactions[0][0].target, pre[0].target);
        assert_eq!(settle.interactions[0][0].callData, pre[0].callData);

        // interactions[1] = the two trampoline intra-interactions (unchanged)
        assert_eq!(
            settle.interactions[1].len(),
            2,
            "two trampoline intra-interactions expected",
        );

        // interactions[2] = post-hooks
        assert_eq!(
            settle.interactions[2].len(),
            2,
            "two post-hook interactions expected",
        );
        assert_eq!(settle.interactions[2][0].target, post[0].target);
        assert_eq!(settle.interactions[2][0].callData, post[0].callData);
        assert_eq!(settle.interactions[2][1].target, post[1].target);
        assert_eq!(settle.interactions[2][1].callData, post[1].callData);
    }
}
