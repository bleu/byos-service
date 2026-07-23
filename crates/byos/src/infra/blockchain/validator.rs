//! Composite proposal validator and simulation validator.
//!
//! [`SimulationValidator`] simulates proposals by calling
//! `trampoline.execute()` via `eth_estimateGas` with a balance state override
//! that gives the trampoline the sell tokens it needs.
//!
//! [`ProposalValidator`] composes
//! [`EscrowValidator`](super::escrow::EscrowValidator)
//! and [`SimulationValidator`] in sequence: escrow first (cheap cached read),
//! then simulation (expensive RPC call).

use {
    super::{
        balance_override::{self, BalanceSlotDetector},
        escrow::EscrowValidator,
    },
    crate::domain::{
        proposal::Proposal,
        validator::{RejectionReason, ValidateProposal, Verdict},
    },
    alloy::{
        primitives::{Address, B256, U256},
        providers::Provider,
        rpc::types::TransactionRequest,
        sol_types::SolCall,
        transports::RpcError,
    },
    byos_common::contracts::{Trampoline, TrampolineFactory},
    parking_lot::Mutex,
    std::collections::HashMap,
};

// ---------------------------------------------------------------------------
// SimulationValidator
// ---------------------------------------------------------------------------

/// Validates proposals by simulating `trampoline.execute()` via
/// `eth_estimateGas` with a balance state override. Also resolves trampoline
/// addresses via `TrampolineFactory.addressOf(sub_solver)` and caches them
/// per sub-solver.
/// Storage slot of `AccessControl._roles` in the Escrow contract. Determined
/// via `forge inspect Escrow storage-layout`.
const ESCROW_ROLES_SLOT: U256 = U256::from_limbs([5, 0, 0, 0]);

/// `keccak256("SUBMITTER_ROLE")` — the role that gates settlement submission
/// in the Trampoline contract.
const SUBMITTER_ROLE: B256 =
    alloy::primitives::b256!("e1a65d1a914580ff6931bc952f0fb26573e9282358a4458bceb9ccc6d923d041");

pub struct SimulationValidator<P> {
    provider: P,
    settlement_address: Address,
    trampoline_factory: Address,
    escrow_address: Address,
    /// Cached trampoline addresses: sub_solver → trampoline. Persistent across
    /// ticks (trampoline addresses are deterministic and never change).
    trampoline_cache: Mutex<HashMap<Address, Address>>,
    /// Detects the ERC-20 storage slot for `balanceOf` and builds state
    /// overrides. Cached per sell token (storage layouts are immutable).
    balance_detector: BalanceSlotDetector<P>,
}

impl<P: Provider + Clone> SimulationValidator<P> {
    pub fn new(
        provider: P,
        settlement_address: Address,
        trampoline_factory: Address,
        escrow_address: Address,
    ) -> Self {
        let balance_detector =
            BalanceSlotDetector::new(provider.clone(), balance_override::DEFAULT_PROBING_DEPTH);
        Self {
            provider,
            settlement_address,
            trampoline_factory,
            escrow_address,
            trampoline_cache: Mutex::new(HashMap::new()),
            balance_detector,
        }
    }

    /// Resolve the trampoline address for a sub-solver. Returns from cache if
    /// available; otherwise calls `TrampolineFactory.addressOf` via RPC.
    async fn resolve_trampoline(
        &self,
        sub_solver: Address,
    ) -> Result<Address, alloy::contract::Error> {
        if let Some(&addr) = self.trampoline_cache.lock().get(&sub_solver) {
            return Ok(addr);
        }

        let factory = TrampolineFactory::new(self.trampoline_factory, &self.provider);
        let addr = factory.addressOf(sub_solver).call().await?;

        self.trampoline_cache.lock().insert(sub_solver, addr);
        Ok(addr)
    }
}

impl<P: Provider + Clone + Send + Sync> ValidateProposal for SimulationValidator<P> {
    async fn validate(&self, proposal: &Proposal) -> Option<Verdict> {
        // 1. Resolve trampoline address. If already stored on the proposal
        //    (re-validation), skip the RPC call; otherwise resolve from the factory (or
        //    its cache).
        let trampoline = match proposal.trampoline {
            Some(addr) => addr,
            None => match self.resolve_trampoline(proposal.sub_solver).await {
                Ok(addr) => addr,
                Err(e) if is_trampoline_revert(&e) => {
                    tracing::info!(
                        id = %proposal.id,
                        sub_solver = %proposal.sub_solver,
                        error = %e,
                        "trampoline resolution reverted, marking SimFailed",
                    );
                    return Some(Verdict::SimFailed);
                }
                Err(e) => {
                    tracing::warn!(
                        id = %proposal.id,
                        sub_solver = %proposal.sub_solver,
                        error = %e,
                        "trampoline resolution failed (transient), deferring to next tick",
                    );
                    return None;
                }
            },
        };

        // 2. Detect balance slot for the sell token and build a state override that
        //    gives the trampoline the sell tokens it needs.
        let Some(strategy) = self
            .balance_detector
            .detect(proposal.sell_token, trampoline)
            .await
        else {
            tracing::info!(
                id = %proposal.id,
                sell_token = %proposal.sell_token,
                "sell token uses unsupported storage layout, rejecting",
            );
            return Some(Verdict::Reject(RejectionReason::UnsupportedToken));
        };

        let (override_addr, account_override) = balance_override::build_override(
            &strategy,
            proposal.sell_token,
            &trampoline,
            &proposal.sell_amount,
        );

        // 3. Build trampoline.execute() calldata.
        let on_chain_proposal = byos_common::contracts::Proposal {
            orderUidHash: proposal.order_uid_hash,
            sellAmount: proposal.sell_amount,
            buyAmount: proposal.buy_amount,
            validUntil: proposal.valid_until,
            nonce: proposal.nonce,
        };

        let calldata = Trampoline::executeCall {
            _proposal: on_chain_proposal,
            _interactions: proposal.interactions.clone(),
            _buyToken: proposal.buy_token,
            _signature: proposal.signature.clone(),
        }
        .abi_encode();

        // 4. Build escrow state override: grant SUBMITTER_ROLE to settlement_address.
        //    In eth_estimateGas, tx.origin == from == settlement_address. The
        //    Trampoline checks `hasRole(SUBMITTER_ROLE, tx.origin)` on the Escrow, so
        //    we override that storage slot to return true.
        let (escrow_addr, escrow_override) = balance_override::build_access_control_override(
            self.escrow_address,
            ESCROW_ROLES_SLOT,
            SUBMITTER_ROLE,
            self.settlement_address,
        );

        // 5. Dispatch eth_estimateGas with state overrides. from: settlement (passes
        //    Trampoline's OnlySettlement check and tx.origin for submitter role) to:
        //    trampoline
        let tx = TransactionRequest::default()
            .from(self.settlement_address)
            .to(trampoline)
            .input(calldata.into());

        match self
            .provider
            .estimate_gas(tx)
            .account_override(override_addr, account_override)
            .account_override(escrow_addr, escrow_override)
            .await
        {
            Ok(gas) => Some(Verdict::Accept {
                gas_used: Some(gas),
                trampoline: Some(trampoline),
            }),
            Err(e) if is_revert(&e) => {
                tracing::info!(
                    id = %proposal.id,
                    sub_solver = %proposal.sub_solver,
                    error = %e,
                    "simulation reverted",
                );
                Some(Verdict::SimFailed)
            }
            Err(e) => {
                tracing::warn!(
                    id = %proposal.id,
                    sub_solver = %proposal.sub_solver,
                    error = %e,
                    "simulation failed (transient), deferring to next tick",
                );
                None
            }
        }
    }
}

/// Returns `true` when the RPC response indicates an EVM execution revert.
/// Only error code `3` (the Ethereum JSON-RPC "execution reverted" code) is
/// treated as a definitive revert. Other `ErrorResp` codes (rate limiting,
/// gas caps, server errors) are transient and should be deferred.
fn is_revert(e: &alloy::transports::RpcError<alloy::transports::TransportErrorKind>) -> bool {
    match e {
        RpcError::ErrorResp(payload) => payload.code == 3,
        RpcError::NullResp => true,
        _ => false,
    }
}

/// Returns `true` when a trampoline resolution error is a real failure
/// (contract revert) rather than a transient transport error. Same
/// classification as [`is_revert`] but operating on `alloy::contract::Error`
/// (which wraps the transport layer).
fn is_trampoline_revert(e: &alloy::contract::Error) -> bool {
    match e {
        alloy::contract::Error::TransportError(rpc_err) => is_revert(rpc_err),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// ProposalValidator (composite)
// ---------------------------------------------------------------------------

/// The production validator: runs [`EscrowValidator`] first (cheap cached
/// read), then [`SimulationValidator`] (expensive `eth_estimateGas`).
/// Short-circuits on the first non-`Accept` verdict.
pub struct ProposalValidator<P> {
    escrow: EscrowValidator<P>,
    simulation: SimulationValidator<P>,
}

impl<P: Provider + Clone> ProposalValidator<P> {
    pub fn new(escrow: EscrowValidator<P>, simulation: SimulationValidator<P>) -> Self {
        Self { escrow, simulation }
    }
}

impl<P: Provider + Clone + Send + Sync> ValidateProposal for ProposalValidator<P> {
    fn begin_tick(&self) {
        self.escrow.begin_tick();
        // Simulation trampoline cache is persistent — no per-tick clearing.
    }

    async fn validate(&self, proposal: &Proposal) -> Option<Verdict> {
        // 1. Escrow check (cheap, cached).
        let escrow_verdict = self.escrow.validate(proposal).await;
        match escrow_verdict {
            Some(Verdict::Accept { .. }) => { /* continue to simulation */ }
            _ => return escrow_verdict, // Reject, SimFailed, or None (deferred)
        }

        // 2. Simulation (expensive, RPC).
        self.simulation.validate(proposal).await
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::domain::proposal::{OrderUid, ProposalStatus, test_proposal},
        alloy::primitives::address,
    };

    // -----------------------------------------------------------------------
    // SimulationValidator
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn simulation_returns_none_on_transport_error() {
        // Provider pointed at a port that is (almost certainly) not listening.
        let provider = alloy::providers::ProviderBuilder::new()
            .connect_http("http://127.0.0.1:1".parse().unwrap());
        let validator = SimulationValidator::new(
            provider,
            address!("9008D19f58AAbD9eD0D60971565AA8510560ab41"),
            address!("0000000000000000000000000000000000000042"),
            address!("0000000000000000000000000000000000000099"),
        );

        let mut proposal = test_proposal(
            OrderUid([0xaa; 56]),
            address!("0000000000000000000000000000000000000001"),
            ProposalStatus::Submitted,
        );
        proposal.sell_token = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        proposal.buy_token = address!("6B175474E89094C44Da98b954EedeAC495271d0F");

        let verdict = validator.validate(&proposal).await;
        assert_eq!(verdict, None, "transport error should defer judgment");
    }

    #[test]
    fn trampoline_cache_returns_stored_address() {
        let provider = alloy::providers::ProviderBuilder::new()
            .connect_http("http://127.0.0.1:1".parse().unwrap());
        let validator =
            SimulationValidator::new(provider, Address::ZERO, Address::ZERO, Address::ZERO);

        let sub_solver = address!("0000000000000000000000000000000000000001");
        let trampoline = address!("0000000000000000000000000000000000000099");

        // Pre-populate cache.
        validator
            .trampoline_cache
            .lock()
            .insert(sub_solver, trampoline);

        // Verify cache hit (sync check, no RPC needed).
        let cached = validator.trampoline_cache.lock().get(&sub_solver).copied();
        assert_eq!(cached, Some(trampoline));
    }

    #[test]
    fn is_revert_classifies_null_resp_as_revert() {
        assert!(is_revert(&RpcError::NullResp));
    }

    #[test]
    fn is_revert_classifies_code_3_as_revert() {
        let payload = alloy::rpc::json_rpc::ErrorPayload {
            code: 3,
            message: "execution reverted".into(),
            data: None,
        };
        assert!(is_revert(&RpcError::ErrorResp(payload)));
    }

    #[test]
    fn is_revert_defers_rate_limit_error() {
        let payload = alloy::rpc::json_rpc::ErrorPayload {
            code: 429,
            message: "rate limit exceeded".into(),
            data: None,
        };
        assert!(!is_revert(&RpcError::ErrorResp(payload)));
    }

    #[test]
    fn is_revert_classifies_transport_error_as_not_revert() {
        let transport = alloy::transports::TransportErrorKind::custom(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ));
        assert!(!is_revert(&transport));
    }

    // -----------------------------------------------------------------------
    // is_trampoline_revert
    // -----------------------------------------------------------------------

    #[test]
    fn trampoline_null_resp_is_revert() {
        let err = alloy::contract::Error::TransportError(RpcError::NullResp);
        assert!(is_trampoline_revert(&err));
    }

    #[test]
    fn trampoline_transport_error_is_not_revert() {
        let err =
            alloy::contract::Error::TransportError(alloy::transports::TransportErrorKind::custom(
                std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
            ));
        assert!(!is_trampoline_revert(&err));
    }
}
