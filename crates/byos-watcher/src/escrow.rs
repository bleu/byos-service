//! Escrow operator: consumes chain watcher output and submits on-chain
//! transactions to the Escrow contract.
//!
//! **Track A (automated):** on revert detection, debit `gas + c_l` from the
//! responsible sub-solver's escrow.
//!
//! **Track B (manual trigger):** freeze/unfreeze/debit on receipt of a CoW
//! core team notification.

use {
    crate::watcher::SettlementOutcome,
    alloy::primitives::{Address, U256},
};

/// The lower reward cap `c_l` — the max revert penalty beyond gas cost.
/// Per contracts ADR-0004: 0.010 ETH mainnet, 10 xDAI Gnosis.
#[derive(Debug, Clone)]
pub struct PenaltyConfig {
    /// `c_l` value in native token (wei).
    pub c_l: U256,
    /// Fraction of `c_l` for non-settlement penalty (0.1 * c_l).
    pub non_settlement_fraction: U256,
}

impl PenaltyConfig {
    /// Mainnet defaults: `c_l` = 0.010 ETH = 10^16 wei.
    pub fn mainnet() -> Self {
        Self {
            c_l: U256::from(10_000_000_000_000_000u64), // 0.01 ETH
            non_settlement_fraction: U256::from(1_000_000_000_000_000u64), // 0.001 ETH
        }
    }

    /// Gnosis defaults: `c_l` = 10 xDAI = 10^19 wei.
    pub fn gnosis() -> Self {
        Self {
            c_l: U256::from(10_000_000_000_000_000_000u128), // 10 xDAI
            non_settlement_fraction: U256::from(1_000_000_000_000_000_000u64), // 1 xDAI
        }
    }
}

/// An escrow operation to be submitted on-chain.
#[derive(Debug, Clone)]
pub enum EscrowAction {
    /// Track A: debit `gas_penalty + c_l` from escrow.
    Debit {
        sub_solver: Address,
        amount: U256,
        /// The settlement tx hash, used as the on-chain `reason` parameter.
        reason: alloy::primitives::B256,
    },
    /// Track B: freeze a sub-solver's escrow during investigation.
    Freeze { sub_solver: Address },
    /// Track B: unfreeze after resolution.
    Unfreeze { sub_solver: Address },
}

/// Compute the Track A debit action for a reverted settlement.
///
/// Penalty = `gas_used * gas_price + c_l` (per ADR-0003, contracts ADR-0004).
pub fn track_a_debit(outcome: &SettlementOutcome, config: &PenaltyConfig) -> Option<EscrowAction> {
    let (sub_solver, gas_penalty, tx_hash) = match outcome {
        SettlementOutcome::Revert {
            sub_solver,
            tx_hash,
            gas_used,
            gas_price,
            ..
        } => {
            let gas_penalty = U256::from(*gas_used).saturating_mul(*gas_price);
            (*sub_solver, gas_penalty, *tx_hash)
        }
        SettlementOutcome::Success { .. } => return None,
    };

    let total = gas_penalty.saturating_add(config.c_l);

    Some(EscrowAction::Debit {
        sub_solver,
        amount: total,
        reason: tx_hash,
    })
}

/// Compute the non-settlement penalty (BYOS won auction but chose not to
/// settle). Penalty = 0.1 * c_l.
pub fn non_settlement_debit(
    sub_solver: Address,
    reason: alloy::primitives::B256,
    config: &PenaltyConfig,
) -> EscrowAction {
    EscrowAction::Debit {
        sub_solver,
        amount: config.non_settlement_fraction,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        alloy::primitives::{B256, address},
    };

    #[test]
    fn track_a_debit_for_revert() {
        let outcome = SettlementOutcome::Revert {
            sub_solver: address!("0000000000000000000000000000000000000001"),
            tx_hash: B256::ZERO,
            block_number: 100,
            gas_used: 200_000,
            gas_price: U256::from(30_000_000_000u64), // 30 gwei
        };
        let config = PenaltyConfig::mainnet();

        let action = track_a_debit(&outcome, &config).unwrap();
        match action {
            EscrowAction::Debit { amount, .. } => {
                // gas = 200k * 30 gwei = 6_000_000_000_000_000
                // c_l = 10_000_000_000_000_000
                // total = 16_000_000_000_000_000 = 0.016 ETH
                assert_eq!(amount, U256::from(16_000_000_000_000_000u64));
            }
            _ => panic!("expected Debit"),
        }
    }

    #[test]
    fn track_a_no_debit_for_success() {
        let outcome = SettlementOutcome::Success {
            sub_solver: address!("0000000000000000000000000000000000000001"),
            tx_hash: B256::ZERO,
            block_number: 100,
        };
        let config = PenaltyConfig::mainnet();

        assert!(track_a_debit(&outcome, &config).is_none());
    }

    #[test]
    fn non_settlement_penalty_is_tenth_of_c_l() {
        let config = PenaltyConfig::mainnet();
        let action = non_settlement_debit(
            address!("0000000000000000000000000000000000000001"),
            B256::ZERO,
            &config,
        );
        match action {
            EscrowAction::Debit { amount, .. } => {
                // 0.1 * 0.01 ETH = 0.001 ETH
                assert_eq!(amount, U256::from(1_000_000_000_000_000u64));
            }
            _ => panic!("expected Debit"),
        }
    }

    #[test]
    fn gnosis_c_l_is_10_xdai() {
        let config = PenaltyConfig::gnosis();
        assert_eq!(config.c_l, U256::from(10_000_000_000_000_000_000u128));
    }
}
