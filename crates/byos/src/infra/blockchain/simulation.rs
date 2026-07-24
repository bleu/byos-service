//! Settlement-harness simulation via `eth_estimateGas` with state overrides.
//!
//! Instead of building a fake `settle()` calldata, we override the settlement
//! contract's code with a minimal harness that:
//!
//! 1. `sellToken.transferFrom(msg.sender, trampoline, sellAmount)` — the user
//!    has already approved the settlement contract, and our harness lives at
//!    that address.
//! 2. `trampoline.execute(proposal, interactions, buyToken, signature)` — the
//!    trampoline sees `msg.sender == SETTLEMENT`, passing its access check.
//!
//! This avoids any ERC-20 balance-slot detection: the user's real token balance
//! and existing settlement approval are exercised directly.
//!
//! The only state override needed beyond the settlement code is granting
//! `SUBMITTER_ROLE` to the user on the Escrow contract, so the trampoline's
//! `tx.origin` check passes.

use {
    alloy::{
        primitives::{Address, Bytes, B256},
        rpc::types::{state::AccountOverride, TransactionRequest},
        sol_types::SolCall,
    },
    byos_common::contracts::{Interaction, Proposal},
    std::collections::HashMap,
};

// ── Harness ABI ──────────────────────────────────────────────────────────────

// We only need the function signature for calldata encoding. The bytecode is
// a constant compiled from the Solidity source below.
//
// ```solidity
// contract SimulationHarness {
//     function simulate(
//         address _sellToken,
//         uint256 _sellAmount,
//         address _trampoline,
//         ITrampoline.Proposal calldata _proposal,
//         ITrampoline.Interaction[] calldata _interactions,
//         address _buyToken,
//         bytes calldata _signature
//     ) external {
//         IERC20(_sellToken).transferFrom(msg.sender, _trampoline, _sellAmount);
//         ITrampoline(_trampoline).execute(_proposal, _interactions, _buyToken, _signature);
//     }
// }
// ```
alloy::sol! {
    struct HarnessProposal {
        bytes32 orderUidHash;
        uint256 sellAmount;
        uint256 buyAmount;
        uint256 validUntil;
        uint256 nonce;
    }

    struct HarnessInteraction {
        address target;
        uint256 value;
        bytes callData;
    }

    function simulate(
        address sellToken,
        uint256 sellAmount,
        address trampoline,
        HarnessProposal proposal,
        HarnessInteraction[] interactions,
        address buyToken,
        bytes signature
    ) external;
}

/// Runtime bytecode of SimulationHarness compiled with solc 0.8.30 (optimizer
/// enabled, 1 000 000 runs, cancun EVM). Produced from the Solidity source
/// documented above. The contract is never deployed on-chain — it is injected
/// via `eth_call`/`eth_estimateGas` code overrides at the settlement address.
const HARNESS_RUNTIME_BYTECODE: &str = "0x608060405234801561000f575f5ffd5b5060043610610029575f3560e01c8063aa3439c91461002d575b5f5ffd5b61004061003b3660046101e0565b610042565b005b6040517f23b872dd00000000000000000000000000000000000000000000000000000000815233600482015273ffffffffffffffffffffffffffffffffffffffff8881166024830152604482018a90528a16906323b872dd906064016020604051808303815f875af11580156100ba573d5f5f3e3d5ffd5b505050506040513d601f19601f820116820180604052508101906100de91906102fa565b506040517f3c03f5d800000000000000000000000000000000000000000000000000000000815273ffffffffffffffffffffffffffffffffffffffff881690633c03f5d89061013b90899089908990899089908990600401610367565b5f604051808303815f87803b158015610152575f5ffd5b505af1158015610164573d5f5f3e3d5ffd5b50505050505050505050505050565b803573ffffffffffffffffffffffffffffffffffffffff81168114610196575f5ffd5b919050565b5f5f83601f8401126101ab575f5ffd5b50813567ffffffffffffffff8111156101c2575f5ffd5b6020830191508360208285010111156101d9575f5ffd5b9250929050565b5f5f5f5f5f5f5f5f5f898b036101608112156101fa575f5ffd5b6102038b610173565b995060208b0135985061021860408c01610173565b975060a07fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffa082011215610249575f5ffd5b5060608a0195506101008a013567ffffffffffffffff81111561026a575f5ffd5b8a01601f81018c1361027a575f5ffd5b803567ffffffffffffffff811115610290575f5ffd5b8c60208260051b84010111156102a4575f5ffd5b602091909101955093506102bb6101208b01610173565b92506101408a013567ffffffffffffffff8111156102d7575f5ffd5b6102e38c828d0161019b565b915080935050809150509295985092959850929598565b5f6020828403121561030a575f5ffd5b81518015158114610319575f5ffd5b9392505050565b81835281816020850137505f602082840101525f60207fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe0601f840116840101905092915050565b863581526020808801359082015260408088013590820152606080880135908201526080808801359082015261010060a0820181905281018590525f610120600587901b8301810190830188837fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffa136839003015b8a8210156104d2577ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffee08786030184528235818112610417575f5ffd5b8c0173ffffffffffffffffffffffffffffffffffffffff61043782610173565b168652602081810135908701526040810135368290037fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe1018112610479575f5ffd5b0160208101903567ffffffffffffffff811115610494575f5ffd5b8036038213156104a2575f5ffd5b606060408801526104b7606088018284610320565b965050506020830192506020840193506001820191506103db565b50505073ffffffffffffffffffffffffffffffffffffffff871660c08501525082810360e0840152610505818587610320565b999850505050505050505056fea2646970667358221220466f37d3e19dec79c16699749252c59a1a648538d88f2ee25199a4f617dac6fb64736f6c634300081e0033";

// ── Escrow AccessControl storage constants ───────────────────────────────────

/// OpenZeppelin v5 AccessControl ERC-7201 namespace base slot:
/// `keccak256(abi.encode(uint256(keccak256("openzeppelin.storage.AccessControl")) - 1)) & ~0xff`
const ACCESS_CONTROL_BASE_SLOT: B256 =
    alloy::primitives::b256!("02dd7bc7dec4dceedda775e58dd541e08a116c6c53815c0bd028192f7b626800");

/// `SUBMITTER_ROLE = keccak256("SUBMITTER_ROLE")`
const SUBMITTER_ROLE: B256 =
    alloy::primitives::b256!("e1a65d1a914580ff6931bc952f0fb26573e9282358a4458bceb9ccc6d923d041");

// ── Public API ───────────────────────────────────────────────────────────────

/// Parameters for building the simulation `eth_estimateGas` call.
pub struct SimulationParams {
    pub settlement: Address,
    pub escrow: Address,
    pub sell_token: Address,
    pub buy_token: Address,
    pub trampoline: Address,
    /// The order owner (extracted from `OrderUid::owner()`).
    pub user: Address,
    pub proposal: Proposal,
    pub interactions: Vec<Interaction>,
    pub signature: Bytes,
}

/// The built simulation: a transaction request and the state overrides needed
/// for the harness to execute correctly.
pub struct Simulation {
    pub tx: TransactionRequest,
    pub settlement_override: (Address, AccountOverride),
    pub escrow_override: (Address, AccountOverride),
}

/// Builds the `eth_estimateGas` request for simulating a proposal.
///
/// Returns a [`Simulation`] containing:
/// - A `TransactionRequest` (`from: user`, `to: settlement`) with harness
///   calldata.
/// - A settlement code override injecting the harness runtime bytecode.
/// - An escrow `state_diff` override granting `SUBMITTER_ROLE` to the user.
pub fn build_simulation(params: &SimulationParams) -> Simulation {
    // 1. Encode harness calldata.
    let calldata = simulateCall {
        sellToken: params.sell_token,
        sellAmount: params.proposal.sellAmount,
        trampoline: params.trampoline,
        proposal: HarnessProposal {
            orderUidHash: params.proposal.orderUidHash,
            sellAmount: params.proposal.sellAmount,
            buyAmount: params.proposal.buyAmount,
            validUntil: params.proposal.validUntil,
            nonce: params.proposal.nonce,
        },
        interactions: params
            .interactions
            .iter()
            .map(|i| HarnessInteraction {
                target: i.target,
                value: i.value,
                callData: i.callData.clone(),
            })
            .collect(),
        buyToken: params.buy_token,
        signature: params.signature.clone(),
    }
    .abi_encode();

    // 2. Transaction: user → settlement (harness).
    let tx = TransactionRequest::default()
        .from(params.user)
        .to(params.settlement)
        .input(calldata.into());

    // 3. Settlement code override.
    let harness_bytes =
        alloy::primitives::hex::decode(HARNESS_RUNTIME_BYTECODE).expect("valid hex constant");
    let settlement_override = AccountOverride {
        code: Some(harness_bytes.into()),
        ..Default::default()
    };

    // 4. Escrow state_diff: grant SUBMITTER_ROLE to user (tx.origin).
    let has_role_slot = compute_has_role_slot(SUBMITTER_ROLE, params.user);
    let mut state_diff = HashMap::default();
    state_diff.insert(has_role_slot, B256::with_last_byte(1));
    let escrow_override = AccountOverride {
        state_diff: Some(state_diff),
        ..Default::default()
    };

    Simulation {
        tx,
        settlement_override: (params.settlement, settlement_override),
        escrow_override: (params.escrow, escrow_override),
    }
}

/// Computes the storage slot for
/// `AccessControl._roles[role].hasRole[account]` using OpenZeppelin v5's
/// ERC-7201 namespaced storage layout.
///
/// Layout:
/// - `_roles` mapping is at `ACCESS_CONTROL_BASE_SLOT`
/// - `_roles[role]` → `keccak256(role ‖ baseSlot)` (Solidity mapping)
/// - `.hasRole` is the first field in `RoleData`, same slot as the struct
/// - `.hasRole[account]` → `keccak256(account ‖ roleDataSlot)`
fn compute_has_role_slot(role: B256, account: Address) -> B256 {
    let role_data_slot = alloy::primitives::keccak256(
        [role.as_slice(), ACCESS_CONTROL_BASE_SLOT.as_slice()].concat(),
    );
    alloy::primitives::keccak256(
        [
            B256::left_padding_from(account.as_slice()).as_slice(),
            role_data_slot.as_slice(),
        ]
        .concat(),
    )
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        alloy::primitives::{U256, address, b256},
    };

    fn sample_params() -> SimulationParams {
        SimulationParams {
            settlement: address!("9008D19f58AAbD9eD0D60971565AA8510560ab41"),
            escrow: address!("0000000000000000000000000000000000000EEE"),
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
    fn calldata_has_correct_selector() {
        let sim = build_simulation(&sample_params());
        let calldata = sim.tx.input.input().expect("calldata should be set");

        // simulate() selector = 0xaa3439c9
        assert_eq!(&calldata[..4], &[0xaa, 0x34, 0x39, 0xc9]);
    }

    #[test]
    fn calldata_round_trips() {
        let params = sample_params();
        let sim = build_simulation(&params);
        let calldata = sim.tx.input.input().expect("calldata should be set");

        let decoded =
            simulateCall::abi_decode(calldata).expect("should decode as simulate()");

        assert_eq!(decoded.sellToken, params.sell_token);
        assert_eq!(decoded.sellAmount, params.proposal.sellAmount);
        assert_eq!(decoded.trampoline, params.trampoline);
        assert_eq!(decoded.proposal.orderUidHash, params.proposal.orderUidHash);
        assert_eq!(decoded.proposal.sellAmount, params.proposal.sellAmount);
        assert_eq!(decoded.proposal.buyAmount, params.proposal.buyAmount);
        assert_eq!(decoded.interactions.len(), 1);
        assert_eq!(decoded.buyToken, params.buy_token);
        assert_eq!(decoded.signature, params.signature);
    }

    #[test]
    fn tx_is_from_user_to_settlement() {
        let params = sample_params();
        let sim = build_simulation(&params);

        assert_eq!(sim.tx.from, Some(params.user));
        assert_eq!(sim.tx.to, Some(params.settlement.into()));
    }

    #[test]
    fn settlement_override_injects_harness_code() {
        let params = sample_params();
        let sim = build_simulation(&params);

        let (addr, ovr) = sim.settlement_override;
        assert_eq!(addr, params.settlement);
        assert!(ovr.code.is_some(), "settlement override must set code");
        assert!(ovr.state.is_none());
        assert!(ovr.state_diff.is_none());
    }

    #[test]
    fn escrow_override_grants_submitter_role() {
        let params = sample_params();
        let sim = build_simulation(&params);

        let (addr, ovr) = sim.escrow_override;
        assert_eq!(addr, params.escrow);
        assert!(ovr.code.is_none());

        let state_diff = ovr.state_diff.as_ref().expect("escrow should have state_diff");
        assert_eq!(state_diff.len(), 1, "exactly one slot should be overridden");

        let slot = compute_has_role_slot(SUBMITTER_ROLE, params.user);
        assert_eq!(
            state_diff.get(&slot),
            Some(&B256::with_last_byte(1)),
            "SUBMITTER_ROLE slot should be set to 1 (true)",
        );
    }

    #[test]
    fn has_role_slot_matches_solidity_computation() {
        // Verified against `forge script` output:
        // _roles[SUBMITTER_ROLE] slot = 0x5c3302b5c06292c55b333749a04b055e6741a4d3298d02ec2344f30876d13dfe
        let role_data_slot = alloy::primitives::keccak256(
            [SUBMITTER_ROLE.as_slice(), ACCESS_CONTROL_BASE_SLOT.as_slice()].concat(),
        );
        assert_eq!(
            role_data_slot,
            b256!("5c3302b5c06292c55b333749a04b055e6741a4d3298d02ec2344f30876d13dfe"),
        );
    }
}
