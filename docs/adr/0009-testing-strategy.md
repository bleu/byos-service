# Testing strategy

Status: accepted

> Adopted from [cowprotocol/services](https://github.com/cowprotocol/services): nextest as the runner, filtered ignored-by-default test classes, an in-crate `tests/{cases,setup}` harness for service-level tests, and a dedicated `e2e` crate. Full-stack testing builds on [`cowdao-grants/offline-mode`](https://github.com/cowdao-grants/offline-mode), the offline CoW-stack environment bleu built under a previous grant.

## Context

The service's correctness claims are mostly about pipelines (ingestion validation order, selection filters, attribution) and about integration (does a signed proposal actually settle through the real contracts, and does the real CoW driver accept and win with our solutions?). The reference sub-solver exists partly to make that second class testable.

[ADR-0002](0002-solver-engine.md)'s central claim — BYOS is a vanilla solver engine behind a **standard, unmodified driver** — can only be tested against a real driver+autopilot loop. offline-mode provides exactly that: a docker-compose stack running the production `orderbook`, `autopilot`, `driver`, and `baseline` solver binaries (built from a pinned `cowprotocol/services` submodule) against a local anvil chain where GPv2 and its ecosystem contracts sit at their real mainnet addresses, with the whole chain state committed as a reloadable `anvil-state.json` — deterministic, offline, no RPC key at runtime.

## Decision

Test classes, mirroring services, with e2e split into two tiers:

1. **Unit tests** — inline `#[cfg(test)]` modules next to the code, pure and fast, run on every push via `just test-unit` (`cargo nextest run`). Domain logic (scoring, selection filters, eligibility math, EIP-712 hashing against known vectors from the contracts repo) lives here.
2. **Service-level tests** — in-crate at `crates/byos/src/tests/` with `cases/` (one file per scenario: ingestion rejections, lifecycle drops, solve selection, rate limiting…) and `setup/` (spawns the real axum servers in-process via the `run()` bind channel from [ADR-0005](0005-crate-anatomy-and-layering.md), with a mocked chain). This is services' driver-test pattern: exercise the real HTTP surface, fake the expensive dependencies.
3. **E2e tier 1 — in-process against anvil** — the `e2e` crate: `src/` holds the orchestration library (plain `anvil --load-state` with the prepared state file, start `byos` in-process, drive the reference `subsolver` through proposal → solve → settle → outcome observation → debit, with the harness playing the driver's role at `/solve`), `tests/` holds one file per feature. `#[ignore]`d, single-threaded, run via `just test-e2e`. Fast enough for per-PR CI.
4. **E2e tier 2 — full stack via offline-mode** — the same `e2e` crate, tests name-filtered `full_stack`, run via `just test-e2e-full` against a running offline-mode stack with BYOS plugged in as a competing solver. This is the auction-competition tier: real autopilot cuts auctions every 2s, the real driver calls our `/solve`, baseline competes, settlements land on anvil, and our settlement watcher observes them. It doubles as the standing rehearsal for M2's end-to-end staging test. Runs locally and nightly — not per-PR until prebuilt service images make it cheap (building the services workspace in Docker is 20–60 min cold).

### Plugging BYOS into offline-mode

The integration is configuration, not code — offline-mode's driver mounts its solver config from the host:

- a `[[solver]]` block in `driver.toml` (`name = "byos"`, `endpoint` pointing at our service, driver `account` = anvil account #0, which is already whitelisted in the GPv2 Authenticator);
- a `byos|http://driver/byos|<account-0-address>` entry in the autopilot's `DRIVERS` env;
- a compose service (or host process) for `byos` on the stack's network.

Quoting stays on baseline. Baseline also stays in the auction deliberately: tier-2 tests assert BYOS **wins when a sub-solver's proposal beats baseline and loses when it doesn't**, which is the RFP acceptance-criterion shape. The reference `subsolver` discovers orders from the stack's real orderbook API — the same discovery channel it uses in production.

Vehicle: offline-mode is pinned as a git submodule; this repo carries a small overlay (compose override, `driver.toml` with the byos block, and the state-regeneration step below). Changes generally useful to offline-mode (e.g. a generic "deploy extra contracts" hook) are PR'd upstream — bleu maintains it.

### Chain fixture: one state file for both tiers

The BYOS contracts (Escrow, TrampolineFactory) are **baked into a regenerated `anvil-state.json`**: an added deploy step in offline-mode's pipeline deploys them (via the already-present CREATE2 singleton factory, for stable addresses) and the resulting state file is committed in this repo's overlay. Regeneration needs a mainnet RPC key once (offline-mode fetches original deployment txs/bytecode); afterwards everything is offline again.

Tier 1 loads the **same state file** into a plain anvil — no docker — so both tiers see identical chain state and contract addresses, and the "where do e2e contract artifacts come from" question disappears: the state file is the artifact, regenerated from [`bleu/byos-contracts`](https://github.com/bleu/byos-contracts) releases when the contracts change.

### The services pin is the API anchor

Our `/solve` and `/notify` DTOs target the solver-engine API at offline-mode's `cowprotocol/services` submodule revision (currently `3480ee76`, 2025-12-18). Upgrading the driver API we support is an explicit event: bump the submodule, rebuild images, fix DTO drift. This turns "which driver version does BYOS target?" from an ambient assumption into a pinned, testable fact.

### Runner and conventions

- **cargo-nextest everywhere** — services' stated reason applies unchanged: cargo test handles global state differently and some service-level tests fail under it.
- **Ignored + name-filtered classes instead of feature flags** — expensive tests are selected by name filter (`full_stack` for tier 2) and `--run-ignored ignored-only`, not compile-time features, so one build serves all classes.
- **One stack per suite, snapshot isolation.** Tier-2 tests assume a running stack (fail fast with a clear message if it isn't up, like offline-mode's own Jest suite) and isolate via `evm_snapshot`/`evm_revert` between tests. We do **not** adopt offline-mode's per-test service-restart + postgres-wipe cycle (60–90s per test); our tests rarely need orderbook-DB isolation, and the few that do can restart explicitly.
- **The reference sub-solver is the test client.** e2e tests drive `subsolver` rather than hand-rolled request builders, so the example code shipped to external teams is itself under test — it cannot rot.
- **Test vocabulary follows CONTEXT.md** — `sub_solver` not `solver` for the external party, scenario names using glossary terms.

## Alternatives considered

- **Workspace-level `tests/` integration dir per crate instead of in-crate `src/tests`.** The cargo default. Rejected — services deliberately keeps service-level tests in-crate where they can reach non-public internals of `setup`; we take the same trade.
- **Testing against a mocked driver only (no full-stack tier).** Rejected — the RFP acceptance criteria are end-to-end ("engine wins and settles live orders"), and ADR-0002's unmodified-driver claim is untestable without a real driver. Tier 1 alone would leave the driver integration to staging surprises.
- **cowprotocol/services' own `playground/` compose stack instead of offline-mode.** Rejected — it fork-tests against live RPC and lacks offline-mode's deterministic committed state, mainnet-address contract placement, and order helpers; and bleu already maintains offline-mode.
- **Adopting offline-mode's Jest harness for our tests.** Its helpers (quote/sign/submit/wait) are proven, but rejected — a second language and toolchain for e2e when the `e2e` crate must exist anyway for tier 1; we port the helper logic to Rust in `e2e/src/setup/`.
- **Per-test service-restart isolation (offline-mode's model).** Genuine isolation, rejected as default — tens of seconds per test; snapshot-revert covers chain state, which is what our assertions read.
- **Deploying Escrow/Trampoline at test-setup time instead of baking state.** No state regeneration or RPC key needed. Rejected as the default — per-run deploy cost, addresses vary across runs, and tier 1/tier 2 fixtures drift apart. Kept as the fallback while the contracts are still churning pre-audit.
- **Docker-compose test stack for tier 1.** Services needs it for Postgres; our audit trail is SQLite/flat-file and the chain is anvil with a preloaded state file, so in-process orchestration suffices.

## Consequences

- The full proposal loop — order on the local orderbook → `subsolver` discovers, routes against local Uniswap V2, signs, submits → ingestion → auction → driver `/solve` → settlement → watcher → (forced-revert) Track A debit — is testable on a laptop with no external dependencies.
- e2e tests inherit offline-mode's rough edges: orders need ~3–10% surplus margin for baseline to engage, chain-time drift needs the existing re-sync helpers, and the baseline solver occasionally needs a restart. The harness wraps these (margin defaults, time re-sync) so individual tests don't repeat them.
- The committed anvil state file must be regenerated when the BYOS contracts change — a documented, RPC-keyed procedure, acceptable while contract churn is front-loaded (pre-audit).
- The services submodule pin ages; bumping it is deliberate maintenance, and the DTO tests tell us exactly what broke.
- CI grows in stages: lint + unit + service tests per PR now; tier 1 per PR once the state file exists; tier 2 nightly once service images are prebuilt and cached.
- Single-threaded e2e keeps runs slow but deterministic; the unit/service classes carry the fast-feedback load.
