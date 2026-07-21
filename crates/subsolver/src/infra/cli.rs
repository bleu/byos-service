//! Operational CLI (ADR-0006): every flag doubles as an env var. Behavioral
//! configuration lives in the TOML file passed via `--config`.

use {clap::Parser, std::path::PathBuf};

#[derive(Debug, Parser)]
#[command(
    about = "Reference BYOS sub-solver: routes CoW orders through Uniswap V2 and submits signed \
             proposals"
)]
pub struct Args {
    /// Path to the behavioral TOML config (see config/example.toml).
    #[arg(long, env)]
    pub config: PathBuf,

    /// Base URL of the CoW orderbook, e.g. https://api.cow.fi/mainnet.
    #[arg(long, env)]
    pub orderbook_url: reqwest::Url,

    /// Base URL of the BYOS proposal API.
    #[arg(long, env)]
    pub byos_url: reqwest::Url,

    /// JSON-RPC endpoint for read-only chain queries (pair reserves,
    /// Trampoline address). The sub-solver never sends transactions.
    #[arg(long, env)]
    pub rpc_url: reqwest::Url,

    /// The sub-solver's signing key. The recovered signer is the sub-solver
    /// identity: escrow collateral key and Trampoline CREATE2 salt. Env-only
    /// by convention — never put keys in committed files.
    #[arg(long, env = "SUBSOLVER_PRIVATE_KEY", hide_env_values = true)]
    pub private_key: alloy::signers::local::PrivateKeySigner,

    /// Tracing filter string.
    #[arg(long, env, default_value = "warn,subsolver=debug")]
    pub log: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_flags() {
        let args = Args::parse_from([
            "subsolver",
            "--config",
            "config/example.toml",
            "--orderbook-url",
            "http://localhost:8080",
            "--byos-url",
            "http://localhost:9588",
            "--rpc-url",
            "http://localhost:8545",
            "--private-key",
            "0x00000000000000000000000000000000000000000000000000000000000a11ce",
        ]);
        assert_eq!(args.log, "warn,subsolver=debug");
        assert_eq!(args.orderbook_url.as_str(), "http://localhost:8080/");
    }
}
