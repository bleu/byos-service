//! Escrow balance validator: checks that a sub-solver's on-chain escrow
//! covers the minimum threshold (`gas + c_l`) before activating a proposal.

use {
    crate::domain::{
        proposal::Proposal,
        scoring::GAS_ESTIMATE,
        validator::{ProposalValidator, RejectionReason, Verdict},
    },
    alloy::{
        primitives::{Address, U256},
        providers::Provider,
        transports::RpcError,
    },
    parking_lot::Mutex,
    std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    },
};

/// Validates proposals by checking the sub-solver's escrow balance on-chain.
///
/// Balances are cached per-tick (one RPC call per sub-solver per validation
/// tick, reused for all their proposals in that tick). The cache is cleared
/// by [`begin_tick`](ProposalValidator::begin_tick).
pub struct EscrowValidator<P> {
    provider: P,
    escrow_address: Address,
    min_collateral: U256,
    /// Last-seen auction gas price, shared with `/solve`.
    gas_price: Arc<AtomicU64>,
    /// Per-tick balance cache: populated lazily during validation, cleared at
    /// the start of each tick.
    cache: Mutex<HashMap<Address, U256>>,
}

impl<P> EscrowValidator<P> {
    /// Minimum escrow balance: `gas_estimate * gas_price + min_collateral`.
    fn threshold(&self) -> U256 {
        let gas_price = U256::from(self.gas_price.load(Ordering::Relaxed));
        U256::from(GAS_ESTIMATE)
            .saturating_mul(gas_price)
            .saturating_add(self.min_collateral)
    }

    /// Clear the per-tick balance cache.
    fn clear_cache(&self) {
        self.cache.lock().clear();
    }
}

impl<P: Provider + Clone> EscrowValidator<P> {
    pub fn new(
        provider: P,
        escrow_address: Address,
        min_collateral: U256,
        gas_price: Arc<AtomicU64>,
    ) -> Self {
        Self {
            provider,
            escrow_address,
            min_collateral,
            gas_price,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Fetch the effective balance for a sub-solver, using the per-tick cache.
    async fn get_balance(&self, sub_solver: Address) -> Result<U256, alloy::contract::Error> {
        if let Some(&balance) = self.cache.lock().get(&sub_solver) {
            return Ok(balance);
        }

        let escrow = byos_common::contracts::Escrow::new(self.escrow_address, &self.provider);
        let balance = escrow.effectiveBalance(sub_solver).call().await?;

        self.cache.lock().insert(sub_solver, balance);
        Ok(balance)
    }
}

impl<P: Provider + Clone + Send + Sync> ProposalValidator for EscrowValidator<P> {
    fn begin_tick(&self) {
        self.clear_cache();
    }

    async fn validate(&self, proposal: &Proposal) -> Option<Verdict> {
        let threshold = self.threshold();

        match self.get_balance(proposal.sub_solver).await {
            Ok(balance) => {
                if balance >= threshold {
                    Some(Verdict::Accept)
                } else {
                    tracing::info!(
                        id = %proposal.id,
                        sub_solver = %proposal.sub_solver,
                        %balance,
                        %threshold,
                        "escrow balance below minimum",
                    );
                    Some(Verdict::Reject(RejectionReason::InsufficientEscrow))
                }
            }
            Err(e) if is_server_error(&e) => {
                // The RPC delivered the call and the server rejected it
                // (contract revert, invalid address, etc.) — this is a real
                // signal, not a transient failure.
                tracing::info!(
                    id = %proposal.id,
                    sub_solver = %proposal.sub_solver,
                    error = %e,
                    "escrow effectiveBalance call reverted",
                );
                Some(Verdict::Reject(RejectionReason::InsufficientEscrow))
            }
            Err(e) => {
                // Transport failures (timeout, DNS, connection refused) and
                // unexpected errors (ABI decode, etc.) are deferred — a broken
                // validator should not punish sub-solvers.
                tracing::warn!(
                    id = %proposal.id,
                    sub_solver = %proposal.sub_solver,
                    error = %e,
                    "escrow check failed (transient or unexpected), deferring to next tick",
                );
                None
            }
        }
    }
}

/// Returns `true` when the RPC delivered the call and the server responded
/// with an error (contract revert, invalid params, etc.). These are real
/// signals — the sub-solver's escrow state is the problem, not our infra.
///
/// Everything else (transport failures, ABI decode errors) is NOT a server
/// error and should be deferred rather than used to reject proposals.
fn is_server_error(e: &alloy::contract::Error) -> bool {
    match e {
        alloy::contract::Error::TransportError(rpc_err) => {
            matches!(rpc_err, RpcError::ErrorResp(_) | RpcError::NullResp)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub_validator() -> EscrowValidator<()> {
        EscrowValidator {
            provider: (),
            escrow_address: Address::ZERO,
            min_collateral: U256::from(10_000_000_000_000_000u64), // 0.01 ETH
            gas_price: Arc::new(AtomicU64::new(20_000_000_000)),   // 20 gwei
            cache: Mutex::new(HashMap::new()),
        }
    }

    // -----------------------------------------------------------------------
    // Threshold
    // -----------------------------------------------------------------------

    #[test]
    fn threshold_uses_gas_estimate_times_price_plus_collateral() {
        let validator = stub_validator();
        // 200_000 * 20 gwei + 0.01 ETH = 0.004 ETH + 0.01 ETH = 0.014 ETH
        let expected = U256::from(200_000u64) * U256::from(20_000_000_000u64)
            + U256::from(10_000_000_000_000_000u64);
        assert_eq!(validator.threshold(), expected);
    }

    #[test]
    fn threshold_with_zero_gas_price_equals_min_collateral() {
        let mut validator = stub_validator();
        validator.gas_price = Arc::new(AtomicU64::new(0));
        assert_eq!(validator.threshold(), validator.min_collateral);
    }

    // -----------------------------------------------------------------------
    // Cache
    // -----------------------------------------------------------------------

    #[test]
    fn clear_cache_empties_populated_cache() {
        let validator = stub_validator();
        validator
            .cache
            .lock()
            .insert(Address::repeat_byte(1), U256::from(1_000u64));
        validator
            .cache
            .lock()
            .insert(Address::repeat_byte(2), U256::from(2_000u64));
        assert_eq!(validator.cache.lock().len(), 2);

        validator.clear_cache();
        assert!(validator.cache.lock().is_empty());
    }

    #[test]
    fn clear_cache_on_empty_cache_is_noop() {
        let validator = stub_validator();
        assert!(validator.cache.lock().is_empty());
        validator.clear_cache();
        assert!(validator.cache.lock().is_empty());
    }

    // -----------------------------------------------------------------------
    // is_server_error classification
    // -----------------------------------------------------------------------

    #[test]
    fn transport_level_error_is_not_server_error() {
        let err =
            alloy::contract::Error::TransportError(alloy::transports::TransportErrorKind::custom(
                std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused"),
            ));
        assert!(!is_server_error(&err));
    }

    #[test]
    fn null_resp_is_server_error() {
        let err = alloy::contract::Error::TransportError(RpcError::NullResp);
        assert!(is_server_error(&err));
    }

    #[test]
    fn abi_error_is_not_server_error() {
        // ABI decode errors are bugs in our code, not escrow problems —
        // they should defer, not reject.
        let err = alloy::contract::Error::TransportError(RpcError::DeserError {
            err: serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bad abi",
            )),
            text: "{}".into(),
        });
        assert!(!is_server_error(&err));
    }

    // -----------------------------------------------------------------------
    // validate: transport error → deferred (None)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn validate_returns_none_on_transport_error() {
        use crate::domain::proposal::{OrderUid, ProposalStatus, test_proposal};

        // Provider pointed at a port that is (almost certainly) not listening.
        let provider = alloy::providers::ProviderBuilder::new()
            .connect_http("http://127.0.0.1:1".parse().unwrap());
        let validator = EscrowValidator::new(
            provider,
            Address::repeat_byte(0xee),
            U256::from(1u64),
            Arc::new(AtomicU64::new(1)),
        );

        let proposal = test_proposal(
            OrderUid([0xaa; 56]),
            Address::repeat_byte(0x01),
            ProposalStatus::Submitted,
        );

        let verdict = validator.validate(&proposal).await;
        assert_eq!(verdict, None, "transport error should defer judgment");
    }
}
