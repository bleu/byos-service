//! Trampoline calldata encoding for settlement interactions.
//!
//! Given a proposal and its interactions, produces the two intra-interactions
//! that BYOS inserts into the `settle()` calldata (per contracts ADR-0003):
//!
//! 1. `sellToken.transfer(trampoline, sellAmount)` — push trade capital from
//!    settlement to the sub-solver's Trampoline instance.
//! 2. `trampoline.execute(proposal, interactions, buyToken, signature)` — run
//!    the sub-solver's route.
//!
//! The Trampoline contract's own code handles the settle-back (`buyAmount` of
//! `buyToken` back to the settlement contract) — BYOS does not encode that.

use {
    crate::contracts::{IERC20, ITrampoline, Interaction, Proposal},
    alloy::{
        primitives::{Address, Bytes, U256},
        sol_types::SolCall,
    },
};

/// Encodes the two settlement intra-interactions that wrap a sub-solver's
/// proposal in a Trampoline `execute` call.
///
/// The caller must supply the `trampoline` address (resolved via
/// `ITrampolineFactory.addressOf(subSolver)` or a local cache).
pub fn encode_trampoline_interactions(
    trampoline: Address,
    sell_token: Address,
    sell_amount: U256,
    proposal: &Proposal,
    interactions: &[Interaction],
    buy_token: Address,
    signature: &Bytes,
) -> [Interaction; 2] {
    // 1. ERC20 transfer: settlement → Trampoline
    let transfer_calldata = IERC20::transferCall {
        to: trampoline,
        amount: sell_amount,
    }
    .abi_encode();

    let transfer = Interaction {
        target: sell_token,
        value: U256::ZERO,
        callData: transfer_calldata.into(),
    };

    // 2. Trampoline.execute(proposal, interactions, buyToken, signature)
    let execute_calldata = ITrampoline::executeCall {
        _proposal: proposal.clone(),
        _interactions: interactions.to_vec(),
        _buyToken: buy_token,
        _signature: signature.clone(),
    }
    .abi_encode();

    let execute = Interaction {
        target: trampoline,
        value: U256::ZERO,
        callData: execute_calldata.into(),
    };

    [transfer, execute]
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        alloy::primitives::{address, b256},
    };

    fn sample_proposal() -> Proposal {
        Proposal {
            orderUidHash: b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            sellAmount: U256::from(1_000_000u64),
            buyAmount: U256::from(990_000u64),
            validUntil: U256::from(1_700_000_000u64),
            nonce: U256::from(1u64),
        }
    }

    fn sample_interactions() -> Vec<Interaction> {
        vec![Interaction {
            target: address!("0000000000000000000000000000000000000042"),
            value: U256::ZERO,
            callData: vec![0xab, 0xcd, 0xef].into(),
        }]
    }

    #[test]
    fn transfer_calldata_has_correct_selector() {
        let trampoline = address!("0000000000000000000000000000000000001234");
        let sell_token = address!("0000000000000000000000000000000000005678");
        let proposal = sample_proposal();
        let interactions = sample_interactions();
        let signature = Bytes::from(vec![0u8; 65]);

        let [transfer, _execute] = encode_trampoline_interactions(
            trampoline,
            sell_token,
            proposal.sellAmount,
            &proposal,
            &interactions,
            address!("0000000000000000000000000000000000009abc"),
            &signature,
        );

        assert_eq!(transfer.target, sell_token);
        assert_eq!(transfer.value, U256::ZERO);

        // ERC20.transfer selector = 0xa9059cbb
        let calldata: &[u8] = &transfer.callData;
        assert_eq!(&calldata[..4], &[0xa9, 0x05, 0x9c, 0xbb]);
    }

    #[test]
    fn execute_calldata_has_correct_selector() {
        let trampoline = address!("0000000000000000000000000000000000001234");
        let sell_token = address!("0000000000000000000000000000000000005678");
        let buy_token = address!("0000000000000000000000000000000000009abc");
        let proposal = sample_proposal();
        let interactions = sample_interactions();
        let signature = Bytes::from(vec![0u8; 65]);

        let [_transfer, execute] = encode_trampoline_interactions(
            trampoline,
            sell_token,
            proposal.sellAmount,
            &proposal,
            &interactions,
            buy_token,
            &signature,
        );

        assert_eq!(execute.target, trampoline);
        assert_eq!(execute.value, U256::ZERO);

        // ITrampoline.execute selector
        let expected_selector = &alloy::primitives::keccak256(
            "execute((bytes32,uint256,uint256,uint256,uint256),(address,uint256,bytes)[],address,\
             bytes)",
        )[..4];
        let calldata: &[u8] = &execute.callData;
        assert_eq!(&calldata[..4], expected_selector);
    }

    #[test]
    fn execute_calldata_round_trips() {
        let trampoline = address!("0000000000000000000000000000000000001234");
        let sell_token = address!("0000000000000000000000000000000000005678");
        let buy_token = address!("0000000000000000000000000000000000009abc");
        let proposal = sample_proposal();
        let interactions = sample_interactions();
        let signature = Bytes::from(vec![1u8; 65]);

        let [_transfer, execute] = encode_trampoline_interactions(
            trampoline,
            sell_token,
            proposal.sellAmount,
            &proposal,
            &interactions,
            buy_token,
            &signature,
        );

        // Decode the execute calldata back and verify fields match.
        let decoded = ITrampoline::executeCall::abi_decode(&execute.callData)
            .expect("ABI decode should succeed");

        assert_eq!(decoded._proposal.orderUidHash, proposal.orderUidHash);
        assert_eq!(decoded._proposal.sellAmount, proposal.sellAmount);
        assert_eq!(decoded._proposal.buyAmount, proposal.buyAmount);
        assert_eq!(decoded._proposal.validUntil, proposal.validUntil);
        assert_eq!(decoded._proposal.nonce, proposal.nonce);
        assert_eq!(decoded._interactions.len(), interactions.len());
        assert_eq!(decoded._interactions[0].target, interactions[0].target);
        assert_eq!(decoded._buyToken, buy_token);
        assert_eq!(decoded._signature, signature);
    }

    #[test]
    fn empty_interactions_encodes() {
        let trampoline = address!("0000000000000000000000000000000000001234");
        let sell_token = address!("0000000000000000000000000000000000005678");
        let buy_token = address!("0000000000000000000000000000000000009abc");
        let proposal = sample_proposal();
        let signature = Bytes::from(vec![0u8; 65]);

        let [_transfer, execute] = encode_trampoline_interactions(
            trampoline,
            sell_token,
            proposal.sellAmount,
            &proposal,
            &[],
            buy_token,
            &signature,
        );

        let decoded = ITrampoline::executeCall::abi_decode(&execute.callData)
            .expect("ABI decode should succeed");

        assert!(decoded._interactions.is_empty());
    }
}
