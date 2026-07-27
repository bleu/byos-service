# Configuration & CLI

Status: accepted

> Adopted from [cowprotocol/services](https://github.com/cowprotocol/services); the `solvers` crate (`infra/cli.rs` + `run.rs` + `config/`) is the cleanest example and the closest analogue to `byos`.

## Context

The service needs operational configuration (ports, RPC URLs, log filters) and behavioral configuration (chain, contract addresses, fee rate, rate limits, simulation interval). Services splits these across clap CLI args and TOML files; we adopt that split.

## Decision

- **clap derive for CLI args, every flag doubling as an env var**: `#[arg(long, env, default_value = ...)]`. Standard args mirror services: `--log` (tracing filter string, e.g. default `"warn,byos=debug"`), `--use-json-logs` (default false, true in production), listener addresses, and `--config <path>`.
- **TOML file for behavioral config**, deserialized with serde:
  - `#[serde(rename_all = "kebab-case", deny_unknown_fields)]` — typos in config keys fail startup loudly instead of silently using defaults.
  - `#[serde_as]` adapters for chain types (`HexOrDecimalU256`-style amounts, `DisplayFromStr`), `humantime_serde` for durations (`simulation-interval = "45s"`).
  - `default = "fn"` for every optional field, so the example file can stay minimal.
- **Committed example configs**: `crates/byos/config/example.toml` (and per-chain variants as they appear, e.g. `example.mainnet.toml`, `example.gnosis.toml`), kept in sync with the config structs. Services commits `example.baseline.toml` etc. for the same reason: the example file is the config documentation.
- **Startup sequence in `run.rs`** ([ADR-0005](0005-crate-anatomy-and-layering.md)): parse args → init tracing ([ADR-0008](0008-observability.md)) → log version + full resolved args → load and validate TOML → build and serve.
- **Secrets stay out of TOML**: the escrow operator key and RPC API keys arrive via env-var-backed clap args, never committed files.

BYOS-specific config concerns the TOML must cover from day one: chain id, `GPv2Settlement` / TrampolineFactory / Escrow addresses (GPv2Settlement is the `--settlement-address` CLI arg), `c_l` fallback value ([ADR-0003](0003-slash-attribution-flow.md)), fee rate (default 0, [ADR-0002](0002-solver-engine.md)), rate-limit tiers ([ADR-0001](0001-proposal-api.md)), simulation interval, public and internal listener addresses, audit-trail path/retention.

## Alternatives considered

- **Everything as CLI/env flags (no TOML).** Services' autopilot leans this way historically. Rejected — the escrow-tier tables and per-chain address sets are structured data; flat flags get unwieldy.
- **Everything in TOML (no CLI).** Rejected — log filter, ports, and secrets are deployment concerns that ops override per environment; env-var-backed flags are the k8s-native way.
- **Config crates like `figment`/`config-rs` with layered merging.** Rejected — layered merges make "what config is actually running?" hard to answer. One TOML file + explicit flags, logged at startup, is auditable.
- **Lenient parsing (unknown fields ignored).** Rejected — `deny_unknown_fields` catches typos that would otherwise ship a mis-tuned rate limit silently.

## Consequences

- Every config addition touches three places: struct, example TOML, and (if operational) a clap arg. The example file drift is caught by using it in e2e tests.
- Logging full args at startup means secrets must be typed to redact themselves on Debug (or logged as "set/unset"), a small discipline to keep.
