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
    ///
    /// Not wrapped — a public orderbook URL is not a secret — but hidden from
    /// `--help` anyway, since a keyed mirror or basic-auth host would print
    /// its credentials there.
    #[arg(long, env, hide_env_values = true)]
    pub orderbook_url: reqwest::Url,

    /// Base URL of the BYOS proposal API. Hidden from `--help` for the same
    /// reason as `orderbook_url`.
    #[arg(long, env, hide_env_values = true)]
    pub byos_url: reqwest::Url,

    /// JSON-RPC endpoint for read-only chain queries (pair reserves,
    /// Trampoline address). The sub-solver never sends transactions.
    ///
    /// `hide_env_values` because clap otherwise prints the live value of
    /// `RPC_URL` in `--help`, and ADR-0006 tells operators to pass it that way.
    #[arg(long, env, hide_env_values = true)]
    pub rpc_url: RpcUrl,

    /// The sub-solver's signing key. The recovered signer is the sub-solver
    /// identity: escrow collateral key and Trampoline CREATE2 salt. Env-only
    /// by convention — never put keys in committed files.
    #[arg(long, env = "SUBSOLVER_PRIVATE_KEY", hide_env_values = true)]
    pub private_key: alloy::signers::local::PrivateKeySigner,

    /// Tracing filter string.
    #[arg(long, env, default_value = "warn,subsolver=debug")]
    pub log: String,
}

/// RPC URL wrapper whose `Debug` hides the value — the URL may carry an API
/// key, and `Args` is logged with `?args` at startup (ADR-0006: secrets redact
/// themselves). Same idea as byos's `RpcUrl`, parsed eagerly here so clap
/// reports a bad URL against the flag.
///
/// This covers the startup log and `--help`, not every path the URL can reach:
/// reqwest formats the full URL into its transport errors, which surface in
/// the poll-failure warn and in `main`'s error return. Scrubbing those is a
/// separate change.
#[derive(Clone)]
pub struct RpcUrl(pub reqwest::Url);

impl std::str::FromStr for RpcUrl {
    type Err = <reqwest::Url as std::str::FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

impl std::fmt::Debug for RpcUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_url_debug_hides_an_embedded_api_key() {
        let args = Args::parse_from([
            "subsolver",
            "--config",
            "config/example.toml",
            "--orderbook-url",
            "http://localhost:8080",
            "--byos-url",
            "http://localhost:9585",
            "--rpc-url",
            "https://eth-mainnet.example.com/v2/super-secret-key",
            "--private-key",
            "0x00000000000000000000000000000000000000000000000000000000000a11ce",
        ]);
        // This is the `?args` startup log; nothing in it may carry the key.
        let logged = format!("{args:?}");
        assert!(
            !logged.contains("super-secret-key"),
            "startup log leaked the RPC API key: {logged}"
        );
        // The signing key rides on alloy's hand-written redacting Debug;
        // asserting it here makes that guarantee ours rather than a
        // dependency's, so an upstream regression fails this test.
        assert!(
            !logged.contains("a11ce"),
            "startup log leaked the signing key: {logged}"
        );
        assert_eq!(
            args.rpc_url.0.as_str(),
            "https://eth-mainnet.example.com/v2/super-secret-key",
            "redacting Debug must not change the parsed value"
        );
    }

    #[test]
    fn parses_the_documented_flags() {
        let args = Args::parse_from([
            "subsolver",
            "--config",
            "config/example.toml",
            "--orderbook-url",
            "http://localhost:8080",
            "--byos-url",
            "http://localhost:9585",
            "--rpc-url",
            "http://localhost:8545",
            "--private-key",
            "0x00000000000000000000000000000000000000000000000000000000000a11ce",
        ]);
        assert_eq!(args.log, "warn,subsolver=debug");
        assert_eq!(args.orderbook_url.as_str(), "http://localhost:8080/");
    }
}
