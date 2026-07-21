//! Builds signed proposals from orders: routes the order through a single
//! Uniswap V2 hop executed by the sub-solver's Trampoline, then signs the
//! result via `byos_common::eip712` (the schema is owned by byos-contracts).
//! Public so the e2e harness can compose proposals with arbitrary extra
//! interactions (e.g. routes that revert only at settlement time) — the
//! sub-solver is fully responsible for its route (ADR-0001).

use {
    crate::domain::routing,
    alloy::{
        primitives::{Address, Bytes, U256, keccak256},
        signers::local::PrivateKeySigner,
        sol,
        sol_types::{Eip712Domain, SolCall},
    },
    byos_common::{contracts, contracts::Interaction, eip712},
};

sol! {
    function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) returns (uint256[]);
    function swapTokensForExactTokens(uint256 amountOut, uint256 amountInMax, address[] path, address to, uint256 deadline) returns (uint256[]);
}

/// A fill-or-kill CoW order as the sub-solver sees it: the fields routing
/// needs, nothing more.
#[derive(Clone, Debug)]
pub struct Order {
    pub uid: Bytes,
    pub sell_token: Address,
    pub buy_token: Address,
    pub sell_amount: U256,
    pub buy_amount: U256,
    pub kind: OrderKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderKind {
    Sell,
    Buy,
}

/// Everything besides the order that shapes the route and the signature.
#[derive(Clone, Debug)]
pub struct RouteParams {
    /// Uniswap V2 router the swap goes through.
    pub router: Address,
    /// The sub-solver's Trampoline: executes the route and must end up
    /// holding the buy tokens it settles back.
    pub trampoline: Address,
    /// Pair reserve of the order's sell token.
    pub reserve_sell: U256,
    /// Pair reserve of the order's buy token.
    pub reserve_buy: U256,
    pub valid_until: u64,
    pub nonce: U256,
    /// Appended to the route verbatim and covered by the signature. This is
    /// the e2e harness's injection point for settlement-time misbehavior
    /// (e.g. Track A forced reverts).
    pub extra_interactions: Vec<Interaction>,
}

/// A routed, signed proposal, ready for `POST /proposals`. What goes on the
/// wire is exactly what got signed — the API client only re-encodes it.
#[derive(Clone, Debug)]
pub struct SignedProposal {
    pub order_uid: Bytes,
    pub sell_amount: U256,
    pub buy_amount: U256,
    pub interactions: Vec<Interaction>,
    pub valid_until: u64,
    pub nonce: U256,
    pub signature: Bytes,
}

impl SignedProposal {
    /// The on-chain 5-field struct the EIP-712 signature covers (the sixth
    /// signed field, `interactionsHash`, is recomputed from `interactions`).
    pub fn onchain(&self) -> contracts::Proposal {
        contracts::Proposal {
            orderUidHash: keccak256(&self.order_uid),
            sellAmount: self.sell_amount,
            buyAmount: self.buy_amount,
            validUntil: U256::from(self.valid_until),
            nonce: self.nonce,
        }
    }
}

/// Routes `order` through a single Uniswap V2 hop and signs the result.
/// Returns `None` when the pool cannot beat the order's limit price.
pub async fn build_proposal(
    order: &Order,
    params: &RouteParams,
    domain: &Eip712Domain,
    signer: &PrivateKeySigner,
) -> Option<SignedProposal> {
    let (sell_amount, buy_amount) = match order.kind {
        OrderKind::Sell => {
            let out =
                routing::amount_out(order.sell_amount, params.reserve_sell, params.reserve_buy)?;
            (out >= order.buy_amount).then_some(())?;
            (order.sell_amount, out)
        }
        OrderKind::Buy => {
            let cost =
                routing::amount_in(order.buy_amount, params.reserve_sell, params.reserve_buy)?;
            (cost <= order.sell_amount).then_some(())?;
            (cost, order.buy_amount)
        }
    };

    let deadline = U256::from(params.valid_until);
    let path = vec![order.sell_token, order.buy_token];
    let swap_call_data = match order.kind {
        OrderKind::Sell => swapExactTokensForTokensCall {
            amountIn: sell_amount,
            amountOutMin: buy_amount,
            path,
            to: params.trampoline,
            deadline,
        }
        .abi_encode(),
        OrderKind::Buy => swapTokensForExactTokensCall {
            amountOut: buy_amount,
            amountInMax: sell_amount,
            path,
            to: params.trampoline,
            deadline,
        }
        .abi_encode(),
    };

    let mut interactions = vec![
        Interaction {
            target: order.sell_token,
            value: U256::ZERO,
            callData: contracts::ERC20::approveCall {
                spender: params.router,
                amount: sell_amount,
            }
            .abi_encode()
            .into(),
        },
        Interaction {
            target: params.router,
            value: U256::ZERO,
            callData: swap_call_data.into(),
        },
    ];
    interactions.extend(params.extra_interactions.iter().cloned());

    let mut proposal = SignedProposal {
        order_uid: order.uid.clone(),
        sell_amount,
        buy_amount,
        interactions,
        valid_until: params.valid_until,
        nonce: params.nonce,
        signature: Bytes::new(),
    };
    let signature =
        eip712::sign_proposal(signer, domain, &proposal.onchain(), &proposal.interactions)
            .await
            .expect("in-memory ECDSA signing is infallible");
    proposal.signature = signature.as_bytes().into();

    Some(proposal)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        alloy::{
            primitives::{Address, Bytes, Signature, U256, address},
            signers::local::PrivateKeySigner,
            sol,
            sol_types::SolCall,
        },
    };

    sol! {
        function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) returns (uint256[]);
        function swapTokensForExactTokens(uint256 amountOut, uint256 amountInMax, address[] path, address to, uint256 deadline) returns (uint256[]);
    }

    const ROUTER: Address = address!("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
    const TRAMPOLINE: Address = address!("0x00000000000000000000000000000000f00dbabe");
    const SELL_TOKEN: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const BUY_TOKEN: Address = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

    fn order(kind: OrderKind) -> Order {
        Order {
            uid: vec![0x11; 56].into(),
            sell_token: SELL_TOKEN,
            buy_token: BUY_TOKEN,
            sell_amount: U256::from(1000),
            buy_amount: U256::from(900),
            kind,
        }
    }

    fn params(extra_interactions: Vec<Interaction>) -> RouteParams {
        RouteParams {
            router: ROUTER,
            trampoline: TRAMPOLINE,
            reserve_sell: U256::from(10_000),
            reserve_buy: U256::from(10_000),
            valid_until: 1_750_000_000,
            nonce: U256::from(7),
            extra_interactions,
        }
    }

    fn signer() -> PrivateKeySigner {
        PrivateKeySigner::from_bytes(&U256::from(0xA11CE).into()).unwrap()
    }

    /// Recovers the signer of `proposal` in `domain`, asserting the signature
    /// covers exactly the proposal's own fields and route.
    fn recovered_signer(proposal: &SignedProposal, domain: &Eip712Domain) -> Address {
        let signature = Signature::from_raw(&proposal.signature).unwrap();
        let interactions_hash = eip712::compute_interactions_hash(&proposal.interactions);
        eip712::recover_proposer(&signature, domain, &proposal.onchain(), interactions_hash)
            .unwrap()
    }

    #[tokio::test]
    async fn sell_order_swaps_exact_sell_amount_and_proposes_the_amm_output() {
        let domain = eip712::byos_domain(31337, Address::ZERO);
        let proposal = build_proposal(&order(OrderKind::Sell), &params(vec![]), &domain, &signer())
            .await
            .unwrap();

        // 1000 in at 10000/10000 reserves yields 906 (see routing tests),
        // which beats the 900 limit.
        assert_eq!(proposal.sell_amount, U256::from(1000));
        assert_eq!(proposal.buy_amount, U256::from(906));
        assert_eq!(proposal.valid_until, 1_750_000_000);
        assert_eq!(proposal.nonce, U256::from(7));
        assert_eq!(proposal.order_uid, Bytes::from(vec![0x11; 56]));

        // The route: approve the router, then swap, buy tokens landing on the
        // Trampoline so it can settle them back.
        assert_eq!(proposal.interactions.len(), 2);
        assert_eq!(proposal.interactions[0].target, SELL_TOKEN);
        let approve =
            contracts::ERC20::approveCall::abi_decode(&proposal.interactions[0].callData).unwrap();
        assert_eq!(approve.spender, ROUTER);
        assert_eq!(approve.amount, U256::from(1000));

        assert_eq!(proposal.interactions[1].target, ROUTER);
        let swap =
            swapExactTokensForTokensCall::abi_decode(&proposal.interactions[1].callData).unwrap();
        assert_eq!(swap.amountIn, U256::from(1000));
        assert_eq!(swap.amountOutMin, U256::from(906));
        assert_eq!(swap.path, vec![SELL_TOKEN, BUY_TOKEN]);
        assert_eq!(swap.to, TRAMPOLINE);
        assert_eq!(swap.deadline, U256::from(1_750_000_000u64));

        // The signature is over exactly these fields in the proposal domain.
        assert_eq!(recovered_signer(&proposal, &domain), signer().address());
    }

    #[tokio::test]
    async fn buy_order_swaps_for_exact_buy_amount() {
        let domain = eip712::byos_domain(31337, Address::ZERO);
        let mut order = order(OrderKind::Buy);
        order.buy_amount = U256::from(906);
        let proposal = build_proposal(&order, &params(vec![]), &domain, &signer())
            .await
            .unwrap();

        // Buying exactly 906 costs 1000, within the 1000 sell limit.
        assert_eq!(proposal.sell_amount, U256::from(1000));
        assert_eq!(proposal.buy_amount, U256::from(906));

        let swap =
            swapTokensForExactTokensCall::abi_decode(&proposal.interactions[1].callData).unwrap();
        assert_eq!(swap.amountOut, U256::from(906));
        assert_eq!(swap.amountInMax, U256::from(1000));
    }

    #[tokio::test]
    async fn orders_the_pool_cannot_beat_the_limit_price_of_are_skipped() {
        let domain = eip712::byos_domain(31337, Address::ZERO);
        let mut order = order(OrderKind::Sell);
        // The pool yields 906 for 1000 in; a 907 limit is unfillable.
        order.buy_amount = U256::from(907);
        assert!(
            build_proposal(&order, &params(vec![]), &domain, &signer())
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn extra_interactions_are_appended_to_the_route_and_signed() {
        let domain = eip712::byos_domain(31337, Address::ZERO);
        let extra = Interaction {
            target: address!("0x00000000000000000000000000000000deadbeef"),
            value: U256::ZERO,
            callData: Bytes::from(vec![0xde, 0xad]),
        };
        let proposal = build_proposal(
            &order(OrderKind::Sell),
            &params(vec![extra.clone()]),
            &domain,
            &signer(),
        )
        .await
        .unwrap();

        assert_eq!(proposal.interactions.len(), 3);
        assert_eq!(proposal.interactions[2], extra);

        // The injected interaction is part of the signed route, not a rider.
        assert_eq!(recovered_signer(&proposal, &domain), signer().address());
    }
}
