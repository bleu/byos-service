//! Chain watcher: polls for new blocks, detects settlement transactions by the
//! BYOS solver, parses calldata for Trampoline interactions, and classifies
//! outcomes (success vs revert).
//!
//! Uses polling (not WebSocket) against Anvil fork for M1. Targets every 3-5
//! blocks. Attribution uses per-sub-solver Trampoline CREATE2 address in
//! calldata (per contracts ADR-0004).

use alloy::primitives::{Address, U256};

/// Outcome of a monitored settlement transaction.
#[derive(Debug, Clone)]
pub enum SettlementOutcome {
    /// Transaction succeeded — the sub-solver's proposal was settled.
    Success {
        sub_solver: Address,
        tx_hash: alloy::primitives::TxHash,
        block_number: u64,
    },
    /// Transaction reverted — the sub-solver incurs a Track A penalty.
    Revert {
        sub_solver: Address,
        tx_hash: alloy::primitives::TxHash,
        block_number: u64,
        gas_used: u64,
        gas_price: U256,
    },
}

impl SettlementOutcome {
    /// The sub-solver attributed to this settlement.
    pub fn sub_solver(&self) -> Address {
        match self {
            Self::Success { sub_solver, .. } | Self::Revert { sub_solver, .. } => *sub_solver,
        }
    }

    /// The transaction hash.
    pub fn tx_hash(&self) -> alloy::primitives::TxHash {
        match self {
            Self::Success { tx_hash, .. } | Self::Revert { tx_hash, .. } => *tx_hash,
        }
    }

    /// Compute the Track A penalty for a revert: `gas_used * gas_price`.
    /// Returns `None` for successful settlements.
    pub fn revert_penalty(&self) -> Option<U256> {
        match self {
            Self::Revert {
                gas_used,
                gas_price,
                ..
            } => Some(U256::from(*gas_used).saturating_mul(*gas_price)),
            Self::Success { .. } => None,
        }
    }
}

/// Configuration for the chain watcher.
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// Address of the GPv2Settlement contract.
    pub settlement: Address,
    /// The BYOS solver address that submits settlement transactions.
    pub solver_address: Address,
    /// Poll interval in blocks (e.g. 3-5 for M1).
    pub poll_interval_blocks: u64,
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        alloy::primitives::{B256, address},
    };

    #[test]
    fn revert_penalty_computed_correctly() {
        let outcome = SettlementOutcome::Revert {
            sub_solver: address!("0000000000000000000000000000000000000001"),
            tx_hash: B256::ZERO,
            block_number: 100,
            gas_used: 200_000,
            gas_price: U256::from(30_000_000_000u64), // 30 gwei
        };
        // 200_000 * 30_000_000_000 = 6_000_000_000_000_000 = 0.006 ETH
        let penalty = outcome.revert_penalty().unwrap();
        assert_eq!(penalty, U256::from(6_000_000_000_000_000u64));
    }

    #[test]
    fn success_has_no_penalty() {
        let outcome = SettlementOutcome::Success {
            sub_solver: address!("0000000000000000000000000000000000000001"),
            tx_hash: B256::ZERO,
            block_number: 100,
        };
        assert!(outcome.revert_penalty().is_none());
    }

    #[test]
    fn sub_solver_accessor() {
        let addr = address!("0000000000000000000000000000000000000042");
        let outcome = SettlementOutcome::Success {
            sub_solver: addr,
            tx_hash: B256::ZERO,
            block_number: 100,
        };
        assert_eq!(outcome.sub_solver(), addr);
    }
}
