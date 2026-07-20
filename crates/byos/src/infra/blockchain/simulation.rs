//! Calldata builder for proposal simulation via `eth_call` against
//! GPv2Settlement.
//!
//! Builds a minimal `settle()` calldata with empty trades and three
//! intra-interactions (ADR-0002):
//! 1. `sellToken.transfer(settlement, sellAmount)` — simulation-only
//! 2. `sellToken.transfer(trampoline, sellAmount)` — real BYOS interaction
//! 3. `trampoline.execute(proposal, interactions, buyToken, signature)` — real
//!
//! The actual `eth_call` dispatch (ingestion check) and the periodic
//! re-simulation loop are not yet implemented.

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
    pub trampoline: Address,
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
    // Intra-interaction 0: transfer(settlement, sellAmount).
    // Simulation-only — in production the vault relayer moves tokens into
    // settlement during trade processing.  We keep the two-hop path
    // (user→settlement→trampoline) instead of a direct user→trampoline
    // transfer so the simulation exercises the exact interactions submitted
    // on-chain and catches fee-on-transfer tokens where less than sellAmount
    // lands in settlement.
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

    fn sample_params() -> SimulationParams {
        SimulationParams {
            settlement: address!("9008D19f58AAbD9eD0D60971565AA8510560ab41"),
            sell_token: address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            buy_token: address!("6B175474E89094C44Da98b954EedeAC495271d0F"),
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
    fn first_intra_interaction_is_transfer_to_settlement() {
        let params = sample_params();
        let calldata = build_simulation_calldata(&params);

        let decoded =
            GPv2Settlement::settleCall::abi_decode(&calldata).expect("should decode as settle()");

        let transfer = &decoded.interactions[1][0];
        assert_eq!(transfer.target, params.sell_token);
        assert_eq!(transfer.value, U256::ZERO);

        let transfer_decoded = ERC20::transferCall::abi_decode(&transfer.callData)
            .expect("should decode as ERC20.transfer()");
        assert_eq!(transfer_decoded.to, params.settlement);
        assert_eq!(transfer_decoded.amount, params.proposal.sellAmount);
    }
}
