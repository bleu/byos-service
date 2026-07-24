//! Trampoline-code-override simulation via `eth_estimateGas`.
//!
//! Simulates a proposal by overriding the **user's** address with the
//! trampoline's deployed bytecode and calling `execute()` from the settlement
//! contract. This works because:
//!
//! - `msg.sender == SETTLEMENT`: the settlement is `from`, so the trampoline's
//!   access check passes without modifying the trampoline code.
//! - **The user already holds the sell tokens**: the trampoline code at the
//!   user's address can spend them directly — no transfer step, no approval
//!   override, no balance-slot detection.
//! - **Trampoline immutables are correct**: we copy the real trampoline's
//!   deployed bytecode, which has SUB_SOLVER, SETTLEMENT, DOMAIN_SEPARATOR, and
//!   ESCROW already embedded.
//!
//! The only additional state override is granting `SUBMITTER_ROLE` to the
//! settlement address on the Escrow contract, so the trampoline's `tx.origin`
//! check passes (`tx.origin = from = settlement`).

use {
    alloy::{
        primitives::{Address, B256, Bytes},
        rpc::types::{TransactionRequest, state::AccountOverride},
        sol_types::SolCall,
    },
    byos_common::contracts::{Interaction, Proposal, Trampoline},
    std::collections::HashMap,
};

// ── Escrow AccessControl storage constants ───────────────────────────────────

/// OpenZeppelin v5 AccessControl ERC-7201 namespace base slot:
/// `keccak256(abi.encode(uint256(keccak256("openzeppelin.storage.AccessControl"
/// )) - 1)) & ~0xff`
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
    pub buy_token: Address,
    pub user: Address,
    pub proposal: Proposal,
    pub interactions: Vec<Interaction>,
    pub signature: Bytes,
    /// The trampoline's deployed bytecode, fetched via `eth_getCode`.
    pub trampoline_code: Bytes,
}

/// The built simulation: a transaction request and the state overrides needed
/// for the trampoline code to execute correctly at the user's address.
pub struct Simulation {
    pub tx: TransactionRequest,
    pub user_override: (Address, AccountOverride),
    pub escrow_override: (Address, AccountOverride),
}

/// Builds the `eth_estimateGas` request for simulating a proposal.
///
/// Returns a [`Simulation`] containing:
/// - A `TransactionRequest` (`from: settlement`, `to: user`) with
///   `Trampoline.execute()` calldata.
/// - A user code override injecting the trampoline's deployed bytecode.
/// - An escrow `state_diff` override granting `SUBMITTER_ROLE` to the
///   settlement address (`tx.origin`).
pub fn build_simulation(params: &SimulationParams) -> Simulation {
    // 1. Encode Trampoline.execute() calldata.
    let calldata = Trampoline::executeCall {
        _proposal: params.proposal.clone(),
        _interactions: params.interactions.clone(),
        _buyToken: params.buy_token,
        _signature: params.signature.clone(),
    }
    .abi_encode();

    // 2. Transaction: settlement → user (acting as trampoline).
    let tx = TransactionRequest::default()
        .from(params.settlement)
        .to(params.user)
        .input(calldata.into());

    // 3. User code override: inject the trampoline's deployed bytecode.
    let user_override = AccountOverride {
        code: Some(params.trampoline_code.clone()),
        ..Default::default()
    };

    // 4. Escrow state_diff: grant SUBMITTER_ROLE to settlement (tx.origin).
    let has_role_slot = compute_has_role_slot(SUBMITTER_ROLE, params.settlement);
    let mut state_diff = HashMap::default();
    state_diff.insert(has_role_slot, B256::with_last_byte(1));
    let escrow_override = AccountOverride {
        state_diff: Some(state_diff),
        ..Default::default()
    };

    Simulation {
        tx,
        user_override: (params.user, user_override),
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
            buy_token: address!("6B175474E89094C44Da98b954EedeAC495271d0F"),
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
            trampoline_code: Bytes::from(vec![0x60, 0x80, 0x60, 0x40]),
        }
    }

    #[test]
    fn calldata_is_trampoline_execute() {
        let sim = build_simulation(&sample_params());
        let calldata = sim.tx.input.input().expect("calldata should be set");

        // Trampoline.execute() selector
        let expected_selector = &alloy::primitives::keccak256(
            "execute((bytes32,uint256,uint256,uint256,uint256),(address,uint256,bytes)[],address,\
             bytes)",
        )[..4];
        assert_eq!(&calldata[..4], expected_selector);
    }

    #[test]
    fn calldata_round_trips() {
        let params = sample_params();
        let sim = build_simulation(&params);
        let calldata = sim.tx.input.input().expect("calldata should be set");

        let decoded =
            Trampoline::executeCall::abi_decode(calldata).expect("should decode as execute()");

        assert_eq!(decoded._proposal.orderUidHash, params.proposal.orderUidHash);
        assert_eq!(decoded._proposal.sellAmount, params.proposal.sellAmount);
        assert_eq!(decoded._proposal.buyAmount, params.proposal.buyAmount);
        assert_eq!(decoded._interactions.len(), 1);
        assert_eq!(decoded._buyToken, params.buy_token);
        assert_eq!(decoded._signature, params.signature);
    }

    #[test]
    fn tx_is_from_settlement_to_user() {
        let params = sample_params();
        let sim = build_simulation(&params);

        assert_eq!(sim.tx.from, Some(params.settlement));
        assert_eq!(sim.tx.to, Some(params.user.into()));
    }

    #[test]
    fn user_override_injects_trampoline_code() {
        let params = sample_params();
        let sim = build_simulation(&params);

        let (addr, ovr) = sim.user_override;
        assert_eq!(addr, params.user);
        assert_eq!(
            ovr.code.as_ref().expect("user override must set code"),
            &params.trampoline_code,
        );
        assert!(ovr.state.is_none());
        assert!(ovr.state_diff.is_none());
    }

    #[test]
    fn escrow_override_grants_submitter_role_to_settlement() {
        let params = sample_params();
        let sim = build_simulation(&params);

        let (addr, ovr) = sim.escrow_override;
        assert_eq!(addr, params.escrow);
        assert!(ovr.code.is_none());

        let state_diff = ovr
            .state_diff
            .as_ref()
            .expect("escrow should have state_diff");
        assert_eq!(state_diff.len(), 1, "exactly one slot should be overridden");

        // SUBMITTER_ROLE is granted to settlement (tx.origin), not user.
        let slot = compute_has_role_slot(SUBMITTER_ROLE, params.settlement);
        assert_eq!(
            state_diff.get(&slot),
            Some(&B256::with_last_byte(1)),
            "SUBMITTER_ROLE slot should be set to 1 (true)",
        );
    }

    #[test]
    fn has_role_slot_matches_solidity_computation() {
        // Verified against `forge script` output:
        // _roles[SUBMITTER_ROLE] slot =
        // 0x5c3302b5c06292c55b333749a04b055e6741a4d3298d02ec2344f30876d13dfe
        let role_data_slot = alloy::primitives::keccak256(
            [
                SUBMITTER_ROLE.as_slice(),
                ACCESS_CONTROL_BASE_SLOT.as_slice(),
            ]
            .concat(),
        );
        assert_eq!(
            role_data_slot,
            b256!("5c3302b5c06292c55b333749a04b055e6741a4d3298d02ec2344f30876d13dfe"),
        );
    }
}
