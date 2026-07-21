//! Escrow balance validator: checks that a sub-solver's on-chain escrow
//! covers the minimum threshold (`gas + c_l`) before activating a proposal.

use {
    crate::domain::{
        proposal::Proposal,
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

/// Fixed gas estimate matching the `/solve` hot path.
const GAS_ESTIMATE: u64 = 200_000;

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
        self.cache.lock().clear();
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
            Err(e) if is_transport_error(&e) => {
                tracing::warn!(
                    id = %proposal.id,
                    sub_solver = %proposal.sub_solver,
                    error = %e,
                    "RPC transport error during escrow check, deferring to next tick",
                );
                None
            }
            Err(e) => {
                tracing::warn!(
                    id = %proposal.id,
                    sub_solver = %proposal.sub_solver,
                    error = %e,
                    "escrow effectiveBalance call failed",
                );
                Some(Verdict::Reject(RejectionReason::InsufficientEscrow))
            }
        }
    }
}

/// Transport-level failures (connection refused, timeout, DNS) are retryable.
/// Server responses — including contract reverts (JSON-RPC error code 3) — are
/// not: they indicate the call was delivered and the chain rejected it.
fn is_transport_error(e: &alloy::contract::Error) -> bool {
    match e {
        alloy::contract::Error::TransportError(rpc_err) => {
            matches!(rpc_err, RpcError::Transport(_))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Direct unit test of the threshold calculation.
    #[test]
    fn threshold_uses_gas_estimate_times_price_plus_collateral() {
        let gas_price = Arc::new(AtomicU64::new(20_000_000_000)); // 20 gwei
        let min_collateral = U256::from(10_000_000_000_000_000u64); // 0.01 ETH

        let validator = EscrowValidator {
            provider: (),
            escrow_address: Address::ZERO,
            min_collateral,
            gas_price,
            cache: Mutex::new(HashMap::new()),
        };

        // 200_000 * 20 gwei + 0.01 ETH = 0.004 ETH + 0.01 ETH = 0.014 ETH
        let expected = U256::from(200_000u64) * U256::from(20_000_000_000u64)
            + U256::from(10_000_000_000_000_000u64);
        assert_eq!(validator.threshold(), expected);
    }

    #[test]
    fn threshold_with_zero_gas_price_equals_min_collateral() {
        let gas_price = Arc::new(AtomicU64::new(0));
        let min_collateral = U256::from(10_000_000_000_000_000u64);

        let validator = EscrowValidator {
            provider: (),
            escrow_address: Address::ZERO,
            min_collateral,
            gas_price,
            cache: Mutex::new(HashMap::new()),
        };

        assert_eq!(validator.threshold(), min_collateral);
    }
}
