//! Behavioral TOML config (ADR-0006): chain and contract addresses, routing
//! parameters, and proposal timing. Operational settings (URLs, the signer
//! key, log filter) are CLI/env flags in `infra::cli`. The committed
//! `config/example.toml` is the documentation for this file and is parsed in
//! tests so it cannot drift.

use {
    alloy::primitives::{Address, B256},
    serde::Deserialize,
    std::time::Duration,
};

/// Behavioral configuration, one TOML file per chain.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    pub chain_id: u64,
    /// TrampolineFactory address: EIP-712 `verifyingContract` and the oracle
    /// for this sub-solver's Trampoline instance.
    pub trampoline_factory: Address,
    pub uniswap_router: Address,
    pub uniswap_factory: Address,
    pub pair_init_code_hash: B256,
    /// Proposal lifetime: `valid_until = now + proposal-ttl`.
    #[serde(with = "humantime_serde", default = "default_proposal_ttl")]
    pub proposal_ttl: Duration,
    /// Delay between orderbook polls.
    #[serde(with = "humantime_serde", default = "default_poll_interval")]
    pub poll_interval: Duration,
    /// Dev knob: append an always-reverting interaction to every route, to
    /// exercise revert handling downstream. BYOS ingestion simulation
    /// (COW-1162) rejects such proposals during background validation;
    /// routes that revert only at settlement time (Track A) are composed by
    /// the e2e harness through `Config::extra_interactions` instead.
    #[serde(default)]
    pub append_revert: bool,
}

fn default_proposal_ttl() -> Duration {
    Duration::from_secs(60)
}

fn default_poll_interval() -> Duration {
    Duration::from_secs(2)
}

impl Config {
    pub fn from_toml(contents: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_committed_example_config_parses() {
        let config = Config::from_toml(include_str!("../../config/example.toml")).unwrap();
        assert_eq!(config.chain_id, 1);
        // Optional fields fall back to defaults when the example omits them.
        assert_eq!(config.poll_interval, std::time::Duration::from_secs(2));
        assert!(!config.append_revert);
    }

    #[test]
    fn unknown_keys_fail_loudly_instead_of_silently_defaulting() {
        let example = include_str!("../../config/example.toml");
        let with_typo = format!("{example}\nproposal-tll = \"30s\"\n");
        assert!(Config::from_toml(&with_typo).is_err());
    }
}
