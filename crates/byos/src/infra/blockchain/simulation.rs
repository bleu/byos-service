//! Proposal simulation via `eth_call` against GPv2Settlement.
//!
//! **Ingestion simulation:** after signature + escrow checks pass, build a
//! minimal `settle()` calldata and `eth_call` it. If it reverts, reject the
//! proposal.
//!
//! **Re-simulation loop:** every N blocks, re-run the same simulation for all
//! active proposals. Permanently drop on revert.
//!
//! The simulation uses empty trades and three intra-interactions (ADR-0002):
//! 1. `sellToken.transferFrom(user, settlement, sellAmount)` — simulation-only
//! 2. `sellToken.transfer(trampoline, sellAmount)` — real BYOS interaction
//! 3. `trampoline.execute(proposal, interactions, buyToken, signature)` — real

use {
    alloy::{
        primitives::{Address, Bytes, U256},
        sol_types::SolCall,
    },
    byos_common::contracts::{ERC20, GPv2InteractionData, GPv2Settlement, Interaction, Proposal},
};

/// Parameters needed to build a simulation `settle()` call.
pub struct SimulationParams {
    pub settlement: Address,
    pub sell_token: Address,
    pub buy_token: Address,
    pub user: Address,
    pub trampoline: Address,
    pub proposal: Proposal,
    pub interactions: Vec<Interaction>,
    pub signature: Bytes,
}

/// Builds the `settle()` calldata for simulating a proposal via `eth_call`.
///
/// Uses empty tokens/prices/trades arrays and three intra-interactions.
/// The `transferFrom` in slot 0 is a simulation workaround — in production,
/// GPv2 pulls user tokens via vault relayer during trade processing.
pub fn build_simulation_calldata(params: &SimulationParams) -> Bytes {
    // Intra-interaction 0: transfer(settlement, sellAmount) —
    // simulation-only, moves user tokens into settlement.
    let transfer_from = GPv2InteractionData {
        target: params.sell_token,
        value: U256::ZERO,
        callData: ERC20::transferCall {
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

    #[test]
    fn simulation_calldata_encodes_without_panic() {
        let params = SimulationParams {
            settlement: address!("9008D19f58AAbD9eD0D60971565AA8510560ab41"),
            sell_token: address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            buy_token: address!("6B175474E89094C44Da98b954EedeAC495271d0F"),
            user: address!("0000000000000000000000000000000000000001"),
            trampoline: address!("0000000000000000000000000000000000000002"),
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
        };

        let calldata = build_simulation_calldata(&params);

        // Should start with the settle() selector.
        let settle_selector = &alloy::primitives::keccak256(
            "settle(address[],uint256[],(uint256,uint256,address,uint256,uint256,uint32,bytes32,\
             uint256,uint256,uint256,bytes)[],(address,uint256,bytes)[][3])",
        )[..4];
        assert_eq!(&calldata[..4], settle_selector);
        assert!(calldata.len() > 100, "calldata should be non-trivial");
    }
}
