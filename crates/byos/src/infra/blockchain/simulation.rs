//! Calldata builder for proposal simulation via `eth_estimateGas` against
//! GPv2Settlement.
//!
//! Builds a minimal `settle()` calldata with empty trades and three
//! intra-interactions (ADR-0002):
//! 1. `sellToken.transferFrom(user, settlement, sellAmount)` — simulation-only
//! 2. `sellToken.transfer(trampoline, sellAmount)` — real BYOS interaction
//! 3. `trampoline.execute(proposal, interactions, buyToken, signature)` — real

use {
    alloy::{
        primitives::{Address, Bytes, U256},
        sol_types::SolCall,
    },
    byos_common::contracts::{GPv2InteractionData, GPv2Settlement, Interaction, Proposal},
};

// The upstream cowprotocol-primitives ERC20 binding only includes `transfer`.
// We need `transferFrom` for simulation, so define a minimal local binding.
alloy::sol! {
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}

/// Parameters needed to build a simulation `settle()` call.
pub struct SimulationParams {
    pub settlement: Address,
    pub sell_token: Address,
    pub buy_token: Address,
    pub trampoline: Address,
    /// The order owner — extracted from `OrderUid::owner()`.
    pub user: Address,
    pub proposal: Proposal,
    pub interactions: Vec<Interaction>,
    pub signature: Bytes,
}

/// Builds the `settle()` calldata for simulating a proposal via `eth_call`.
///
/// Uses empty tokens/prices/trades arrays and three intra-interactions.
///
/// # Warning
///
/// This calldata is intended **only** for `eth_call` simulation. It must
/// never be submitted as a real transaction — the first interaction fakes
/// token movement that the vault relayer handles in production.
pub fn build_simulation_calldata(params: &SimulationParams) -> Bytes {
    // Intra-interaction 0: transferFrom(user, settlement, sellAmount).
    // Simulation-only — in production the vault relayer moves tokens into
    // settlement during trade processing. The user has already approved the
    // settlement contract, so transferFrom succeeds if the user holds enough
    // sell tokens. We keep the two-hop path (user→settlement→trampoline)
    // instead of a direct user→trampoline transfer so the simulation
    // exercises the exact interactions submitted on-chain and catches
    // fee-on-transfer tokens where less than sellAmount lands in settlement.
    let transfer_from = GPv2InteractionData {
        target: params.sell_token,
        value: U256::ZERO,
        callData: transferFromCall {
            from: params.user,
            to: params.settlement,
            amount: params.proposal.sellAmount,
        }
        .abi_encode()
        .into(),
    };

    // Intra-interactions 1 & 2: the real BYOS Trampoline interactions.
    let trampoline_interactions = byos_common::trampoline::encode_trampoline_interactions(
        params.trampoline,
        params.sell_token,
        &params.proposal,
        &params.interactions,
        params.buy_token,
        &params.signature,
    );

    let to_gpv2 = |i: Interaction| GPv2InteractionData {
        target: i.target,
        value: i.value,
        callData: i.callData,
    };

    let intra: Vec<GPv2InteractionData> = std::iter::once(transfer_from)
        .chain(trampoline_interactions.into_iter().map(to_gpv2))
        .collect();

    // Build settle() call with empty tokens/prices/trades and our
    // intra-interactions.
    let calldata = GPv2Settlement::settleCall {
        tokens: vec![],
        clearingPrices: vec![],
        trades: vec![],
        interactions: [vec![], intra, vec![]],
    }
    .abi_encode();

    calldata.into()
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        alloy::primitives::{address, b256},
    };

    fn sample_params() -> SimulationParams {
        SimulationParams {
            settlement: address!("9008D19f58AAbD9eD0D60971565AA8510560ab41"),
            sell_token: address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            buy_token: address!("6B175474E89094C44Da98b954EedeAC495271d0F"),
            trampoline: address!("0000000000000000000000000000000000000002"),
            user: address!("0000000000000000000000000000000000000099"),
            proposal: Proposal {
                orderUidHash: b256!(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                ),
                sellAmount: U256::from(1_000_000u64),
                buyAmount: U256::from(990_000u64),
                validUntil: U256::from(1_700_000_000u64),
                nonce: U256::from(1u64),
            },
            interactions: vec![Interaction {
                target: address!("0000000000000000000000000000000000000042"),
                value: U256::ZERO,
                callData: vec![0xab, 0xcd].into(),
            }],
            signature: Bytes::from(vec![0u8; 65]),
        }
    }

    #[test]
    fn settle_calldata_has_correct_structure() {
        let params = sample_params();
        let calldata = build_simulation_calldata(&params);

        let decoded =
            GPv2Settlement::settleCall::abi_decode(&calldata).expect("should decode as settle()");

        assert!(decoded.tokens.is_empty());
        assert!(decoded.clearingPrices.is_empty());
        assert!(decoded.trades.is_empty());

        assert!(
            decoded.interactions[0].is_empty(),
            "pre-interactions should be empty"
        );
        assert_eq!(
            decoded.interactions[1].len(),
            3,
            "intra-interactions: transfer + 2 trampoline"
        );
        assert!(
            decoded.interactions[2].is_empty(),
            "post-interactions should be empty"
        );
    }

    #[test]
    fn first_intra_interaction_is_transfer_from_user_to_settlement() {
        let params = sample_params();
        let calldata = build_simulation_calldata(&params);

        let decoded =
            GPv2Settlement::settleCall::abi_decode(&calldata).expect("should decode as settle()");

        let interaction = &decoded.interactions[1][0];
        assert_eq!(interaction.target, params.sell_token);
        assert_eq!(interaction.value, U256::ZERO);

        // transferFrom(address,address,uint256) selector = 0x23b872dd
        assert_eq!(&interaction.callData[..4], &[0x23, 0xb8, 0x72, 0xdd]);

        let decoded = super::transferFromCall::abi_decode(&interaction.callData)
            .expect("should decode as transferFrom()");
        assert_eq!(decoded.from, params.user);
        assert_eq!(decoded.to, params.settlement);
        assert_eq!(decoded.amount, params.proposal.sellAmount);
    }
}
