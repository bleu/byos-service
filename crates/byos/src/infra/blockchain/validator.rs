//! Composite proposal validator and simulation validator.
//!
//! [`SimulationValidator`] dispatches `eth_estimateGas` with a trampoline
//! code override on the user's address and resolves trampoline addresses
//! (and their bytecode) via `TrampolineFactory.addressOf` + `eth_getCode`.
//!
//! [`ProposalValidator`] composes
//! [`EscrowValidator`](super::escrow::EscrowValidator)
//! and [`SimulationValidator`] in sequence: escrow first (cheap cached read),
//! then simulation (expensive RPC call).

use {
    super::{escrow::EscrowValidator, simulation},
    crate::domain::{
        proposal::Proposal,
        validator::{ValidateProposal, Verdict},
    },
    alloy::{primitives::Address, providers::Provider, transports::RpcError},
    byos_common::contracts::TrampolineFactory,
    parking_lot::Mutex,
    std::collections::HashMap,
};

// ---------------------------------------------------------------------------
// SimulationValidator
// ---------------------------------------------------------------------------

/// Resolved trampoline: address + deployed bytecode.
#[derive(Clone)]
struct TrampolineInfo {
    address: Address,
    code: alloy::primitives::Bytes,
}

/// Validates proposals by simulating them via `eth_estimateGas` with the
/// trampoline's bytecode injected at the user's address. Also resolves
/// trampoline addresses and bytecode via `TrampolineFactory.addressOf` +
/// `eth_getCode`, caching both per sub-solver.
pub struct SimulationValidator<P> {
    provider: P,
    settlement_address: Address,
    escrow_address: Address,
    trampoline_factory: Address,
    /// Cached trampoline info: sub_solver → (address, bytecode). Persistent
    /// across ticks (trampoline addresses and code are deterministic and never
    /// change).
    trampoline_cache: Mutex<HashMap<Address, TrampolineInfo>>,
}

impl<P: Provider + Clone> SimulationValidator<P> {
    pub fn new(
        provider: P,
        settlement_address: Address,
        escrow_address: Address,
        trampoline_factory: Address,
    ) -> Self {
        Self {
            provider,
            settlement_address,
            escrow_address,
            trampoline_factory,
            trampoline_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve the trampoline address and bytecode for a sub-solver. Returns
    /// from cache if available; otherwise calls `TrampolineFactory.addressOf`
    /// and `eth_getCode` via RPC.
    async fn resolve_trampoline(
        &self,
        sub_solver: Address,
    ) -> Result<TrampolineInfo, alloy::contract::Error> {
        if let Some(info) = self.trampoline_cache.lock().get(&sub_solver).cloned() {
            return Ok(info);
        }

        let factory = TrampolineFactory::new(self.trampoline_factory, &self.provider);
        let address = factory.addressOf(sub_solver).call().await?;

        let code = self
            .provider
            .get_code_at(address)
            .await
            .map_err(alloy::contract::Error::TransportError)?;

        let info = TrampolineInfo { address, code };
        self.trampoline_cache
            .lock()
            .insert(sub_solver, info.clone());
        Ok(info)
    }
}

impl<P: Provider + Clone + Send + Sync> ValidateProposal for SimulationValidator<P> {
    async fn validate(&self, proposal: &Proposal) -> Option<Verdict> {
        // 1. Resolve trampoline address and bytecode.
        let cached = self
            .trampoline_cache
            .lock()
            .get(&proposal.sub_solver)
            .cloned();
        let trampoline = match cached {
            Some(info) => info,
            None => match self.resolve_trampoline(proposal.sub_solver).await {
                Ok(info) => info,
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

        // 2. Build simulation (tx + state overrides).
        let on_chain_proposal = byos_common::contracts::Proposal {
            orderUidHash: proposal.order_uid_hash,
            sellAmount: proposal.sell_amount,
            buyAmount: proposal.buy_amount,
            validUntil: proposal.valid_until,
            nonce: proposal.nonce,
        };

        let sim = simulation::build_simulation(&simulation::SimulationParams {
            settlement: self.settlement_address,
            escrow: self.escrow_address,
            buy_token: proposal.buy_token,
            user: proposal.order_uid.owner(),
            proposal: on_chain_proposal,
            interactions: proposal.interactions.clone(),
            signature: proposal.signature.clone(),
            trampoline_code: trampoline.code,
        });

        // 3. Dispatch eth_estimateGas with state overrides.
        match self
            .provider
            .estimate_gas(sim.tx)
            .account_override(sim.user_override.0, sim.user_override.1)
            .account_override(sim.escrow_override.0, sim.escrow_override.1)
            .await
        {
            Ok(gas) => Some(Verdict::Accept {
                gas_used: Some(gas),
                trampoline: Some(trampoline.address),
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
            address!("0000000000000000000000000000000000000EEE"),
            address!("0000000000000000000000000000000000000042"),
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
    fn trampoline_cache_returns_stored_info() {
        let provider = alloy::providers::ProviderBuilder::new()
            .connect_http("http://127.0.0.1:1".parse().unwrap());
        let validator =
            SimulationValidator::new(provider, Address::ZERO, Address::ZERO, Address::ZERO);

        let sub_solver = address!("0000000000000000000000000000000000000001");
        let trampoline_addr = address!("0000000000000000000000000000000000000099");
        let code = alloy::primitives::Bytes::from(vec![0x60, 0x80]);

        // Pre-populate cache.
        validator.trampoline_cache.lock().insert(
            sub_solver,
            TrampolineInfo {
                address: trampoline_addr,
                code: code.clone(),
            },
        );

        // Verify cache hit (sync check, no RPC needed).
        let cached = validator.trampoline_cache.lock().get(&sub_solver).cloned();
        assert!(cached.is_some());
        let info = cached.unwrap();
        assert_eq!(info.address, trampoline_addr);
        assert_eq!(info.code, code);
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
