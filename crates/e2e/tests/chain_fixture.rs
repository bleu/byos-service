//! Tier-1 chain fixture smoke test (ADR-0009): plain anvil loaded
//! with the offline-mode state file, plus the BYOS contracts deployed through
//! the CREATE2 singleton factory at suite start.

use {
    alloy::{
        primitives::{Address, U256},
        providers::Provider,
    },
    e2e::chain::Chain,
};

#[tokio::test]
#[ignore = "tier-1 e2e: needs anvil and the offline-mode submodule"]
async fn chain_fixture_boots_with_byos_contracts() {
    let chain = Chain::spawn().await.expect("chain fixture boots");

    // The BYOS contracts landed at their CREATE2-derived addresses.
    let escrow_code = chain
        .provider()
        .get_code_at(chain.escrow)
        .await
        .expect("read code at the Escrow address");
    assert!(
        !escrow_code.is_empty(),
        "no contract code at the Escrow CREATE2 address"
    );
    let factory_code = chain
        .provider()
        .get_code_at(chain.trampoline_factory)
        .await
        .expect("read code at the TrampolineFactory address");
    assert!(
        !factory_code.is_empty(),
        "no contract code at the TrampolineFactory address"
    );

    // The Escrow answers reads: an unknown sub-solver has zero balance.
    let escrow = byos_common::contracts::Escrow::new(chain.escrow, chain.provider());
    let balance = escrow
        .effectiveBalance(Address::repeat_byte(0x42))
        .call()
        .await
        .expect("effectiveBalance read succeeds");
    assert_eq!(
        balance,
        U256::ZERO,
        "unknown sub-solver should have zero effective balance"
    );

    // Tier 1 settles from anvil account 0, so the state must have it
    // whitelisted as a solver in the GPv2 Authenticator.
    assert!(
        chain
            .solver_is_authenticated()
            .await
            .expect("isSolver read succeeds"),
        "anvil account 0 should be whitelisted in the GPv2 Authenticator"
    );
}
