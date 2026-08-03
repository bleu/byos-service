//! Full-settle simulation request builder (ADR-0012).
//!
//! A proposal is simulated as the transaction the driver would actually
//! submit: a one-trade `settle()` carrying the real order and the trampoline
//! interactions ([`byos_common::settlement::encode_settle`]), estimated from
//! a dummy submitter address. Two state overrides stand in for the
//! permissions the dummy lacks:
//!
//! - **Authenticator** → code override with `AnyoneAuthenticator` (any `from`
//!   passes the solver allowlist). Bytecode vendored from
//!   `cowprotocol/services` `contracts/artifacts/AnyoneAuthenticator.json`
//!   (MIT).
//! - **Escrow** → `state_diff` granting `SUBMITTER_ROLE` to the dummy
//!   (`tx.origin`). The Escrow inherits NON-upgradeable OpenZeppelin v5
//!   `AccessControl`, so `_roles` lives at plain storage slot 5 — not the
//!   ERC-7201 namespaced slot. Verified on a mainnet fork and pinned by `forge
//!   inspect Escrow storage-layout` in byos-contracts.

use {
    alloy::{
        primitives::{Address, B256, Bytes, address, b256, hex, keccak256},
        rpc::types::{TransactionRequest, state::AccountOverride},
    },
    byos_common::{
        contracts::{Interaction, Proposal},
        settlement::{CowOrder, encode_settle},
    },
    std::collections::HashMap,
};

/// The simulation's `from` / `tx.origin`. Arbitrary: the overrides grant it
/// everything it needs. Same address CoW's quote verifier uses for its piggy
/// bank, chosen for recognizability in traces.
pub const DUMMY_SUBMITTER: Address = address!("1111111111111111111111111111111111111111");

/// `AnyoneAuthenticator` deployed bytecode (`isSolver(address)` always
/// true), vendored from cowprotocol/services.
const ANYONE_AUTHENTICATOR_CODE: &[u8] = &hex!(
    "6080604052348015600e575f5ffd5b50600436106026575f3560e01c806302cc250d14602a575b5f5ffd5b603b6035366004604f565b50600190565b604051901515815260200160405180910390f35b5f60208284031215605e575f5ffd5b813573ffffffffffffffffffffffffffffffffffffffff811681146080575f5ffd5b939250505056fea164736f6c634300081e000a"
);

/// `keccak256("SUBMITTER_ROLE")` — must match the Escrow contract.
const SUBMITTER_ROLE: B256 =
    b256!("e1a65d1a914580ff6931bc952f0fb26573e9282358a4458bceb9ccc6d923d041");

/// Storage slot of the `AccessControl._roles` mapping in the Escrow: plain
/// slot 5, after ERC20's five slots (non-upgradeable OZ v5 layout).
const ROLES_MAPPING_SLOT: u8 = 5;

/// Parameters for building the simulation `eth_estimateGas` call.
pub struct SimulationParams<'a> {
    pub settlement: Address,
    pub authenticator: Address,
    pub escrow: Address,
    pub trampoline: Address,
    pub order: &'a CowOrder,
    pub proposal: Proposal,
    pub route: &'a [Interaction],
    pub signature: &'a Bytes,
    /// Hook pre-interactions encoded as `HooksTrampoline.execute()` calls.
    pub pre_interactions: Vec<byos_common::contracts::GPv2InteractionData>,
    /// Hook post-interactions encoded as `HooksTrampoline.execute()` calls.
    pub post_interactions: Vec<byos_common::contracts::GPv2InteractionData>,
}

/// The built simulation: the transaction request and the two state
/// overrides it must be estimated under.
pub struct Simulation {
    pub tx: TransactionRequest,
    pub authenticator_override: (Address, AccountOverride),
    pub escrow_override: (Address, AccountOverride),
}

/// Builds the `eth_estimateGas` request simulating a proposal as a real
/// settlement.
pub fn build_simulation(params: &SimulationParams) -> Simulation {
    let calldata = encode_settle(
        params.order,
        &params.proposal,
        params.trampoline,
        params.route,
        params.signature,
        &params.pre_interactions,
        &params.post_interactions,
    );

    let tx = TransactionRequest::default()
        .from(DUMMY_SUBMITTER)
        .to(params.settlement)
        .input(calldata.into());

    let authenticator_override = AccountOverride {
        code: Some(Bytes::from_static(ANYONE_AUTHENTICATOR_CODE)),
        ..Default::default()
    };

    let mut state_diff = HashMap::default();
    state_diff.insert(
        submitter_role_slot(DUMMY_SUBMITTER),
        B256::with_last_byte(1),
    );
    let escrow_override = AccountOverride {
        state_diff: Some(state_diff),
        ..Default::default()
    };

    Simulation {
        tx,
        authenticator_override: (params.authenticator, authenticator_override),
        escrow_override: (params.escrow, escrow_override),
    }
}

/// Computes the storage slot of
/// `AccessControl._roles[SUBMITTER_ROLE].hasRole[account]`:
/// `keccak256(pad32(account) ‖ keccak256(pad32(role) ‖ pad32(5)))`.
fn submitter_role_slot(account: Address) -> B256 {
    let role_data_slot = keccak256(
        [
            SUBMITTER_ROLE.as_slice(),
            B256::with_last_byte(ROLES_MAPPING_SLOT).as_slice(),
        ]
        .concat(),
    );
    keccak256(
        [
            B256::left_padding_from(account.as_slice()).as_slice(),
            role_data_slot.as_slice(),
        ]
        .concat(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submitter_role_slot_matches_forge_computation() {
        // Pinned against `cast keccak` over the same concatenations; the
        // formula itself was verified on a mainnet fork (vm.store at this
        // slot made hasRole pass).
        assert_eq!(
            submitter_role_slot(DUMMY_SUBMITTER),
            b256!("4eb8c5e0e8f6947fc61867e46604b89f6f2511c7f24d1be62be922d32b056655"),
        );
    }
}
