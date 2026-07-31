# BYOS Service

The off-chain service for **Bring Your Own Solver (BYOS)**: a bonded [CoW Protocol](https://cow.fi) solver that sources settlement routes from permissionless external sub-solvers. Sub-solvers submit EIP-712-signed routing proposals against specific order UIDs, collateralized by an on-chain escrow; BYOS scores them, feeds the best into CoW's solver competition through a standard driver, and settles winners through per-sub-solver Trampoline contracts.

Built under a [CoW DAO grant](https://forum.cow.fi/t/grant-application-cow-byos-bring-your-own-solver/3476) answering the [BYOS RFP](https://forum.cow.fi/t/rfp-bring-your-own-solver-byos/3469). The on-chain half (Escrow, Trampoline, TrampolineFactory) lives in [`bleu/byos-contracts`](https://github.com/bleu/byos-contracts).

Status: **in progress** — the proposal API, scoring/solve engine, background validation, proposal lifecycle and retention, audit trail, escrow operator with Track A debits, and reference sub-solver are implemented; the e2e harness is not. The chain watcher is not needed: settlement outcomes come from the driver's notifications (ADR-0010).

## Crates

| Crate | Description | Status |
|---|---|---|
| [`byos`](crates/byos) | The BYOS service: public proposal API + CoW solver engine, one process, two listeners | in progress |
| [`byos-common`](crates/byos-common) | Shared contract bindings, EIP-712 schema, and Trampoline calldata encoding | in progress |
| [`subsolver`](crates/subsolver) | Reference sub-solver: example proposal-API client, also the e2e-test counterpart | implemented |
| [`e2e`](crates/e2e) | End-to-end tests, two tiers: in-process against plain anvil, and full CoW stack via [offline-mode](https://github.com/cowdao-grants/offline-mode) | tier-1 chain fixture |

## Architecture

Sub-solvers discover orders from the public CoW orderbook, compute routes, and `POST` signed proposals to the public listener. Ingestion verifies the signature and stores the proposal; a background loop then runs the escrow check and the settlement simulation and moves it to `active` or rejects it. The CoW driver calls `/solve` on the internal listener; the engine returns the single best proposal per order UID, wrapped in one Trampoline `execute` call, and the driver competes with it as a normal solver. Background loops re-simulate standing proposals, take settlement outcomes from the driver's `/notify`, debit escrow on attributable reverts, and sweep terminal proposals past their retention window.

Start with [`CONTEXT.md`](CONTEXT.md) for the domain language, then the ADRs:

- [ADR-0001](docs/adr/0001-proposal-api.md) — proposal API & sub-solver authorization
- [ADR-0002](docs/adr/0002-solver-engine.md) — solver engine (still proposed; open questions listed inside)
- [ADR-0003](docs/adr/0003-slash-attribution-flow.md) — slashing policy & attribution
- [ADRs 0004–0009](docs/adr/README.md) — engineering conventions ported from [`cowprotocol/services`](https://github.com/cowprotocol/services)

CoW protocol background (solver auctions, slashing policy, CIPs) is captured in [`docs/reference/`](docs/reference), SLO targets in [`docs/metrics-reasoning.md`](docs/metrics-reasoning.md).

## Development

Prerequisites: stable Rust (via [rustup](https://rustup.rs); `rust-toolchain.toml` pins the channel), a nightly toolchain for rustfmt, [`just`](https://github.com/casey/just), and [`cargo-nextest`](https://nexte.st). E2e tests additionally need [Foundry](https://getfoundry.sh)'s anvil and the offline-mode submodule (`git submodule update --init`), whose committed `anvil-state.json` is the tier-1 chain fixture. Running the service (and the DB-backed tests) needs Postgres — `docker compose up -d postgres` provides one. In production, pass the connection string via the `DATABASE_URL` env var rather than `--database-url`: CLI arguments are visible to other users on the host via `ps`.

```sh
just build          # cargo build --workspace
just test-unit      # cargo nextest run
just test-db        # service-level tests against the compose Postgres
just test-e2e       # e2e tier 1: in-process against plain anvil
just test-e2e-full  # e2e tier 2: against a running offline-mode stack
just clippy         # -D warnings, all features and targets
just lint-openapi   # validate + lint crates/byos/openapi.yml (needs node)
just fmt            # cargo +nightly fmt (never stable fmt)
```

The public proposal API is specified in [`crates/byos/openapi.yml`](crates/byos/openapi.yml). Nothing serves it — render it locally with `npx @redocly/cli build-docs crates/byos/openapi.yml`, which writes a self-contained `redoc-static.html`. That is the same split [`cowprotocol/services`](https://github.com/cowprotocol/services) uses: the spec sits next to the crate, CI validates and lints it, and the rendered docs live outside the repo. The internal listener's `/solve` and `/notify` are deliberately absent from it — they implement CoW's own solver-engine spec.

### Running byos by hand

`just byos-local` in one shell, `just propose` in another. The service boots against the compose Postgres with no chain, so validation is AcceptAll; the helper signs a proposal, submits it, and polls until the background validator flips the status. Watching it go from `submitted` to `active` is the point — that is the checkpoint that says the service itself is fine when a full stack around it misbehaves. Both recipes share the chain id, factory, key and ports as `just` variables, so the two commands cannot disagree on the EIP-712 domain; the signer is anvil account 4.

Three things that look like bugs and are not. A 202 followed by a 404 means the domain disagreement above happened anyway (you overrode a flag): `recover_proposer` only errors on a malformed signature, so a chain-id or factory mismatch recovers a different address instead of failing, the POST succeeds, and the owner-scoped read 404s the id it just handed you (ADR-0011's anti-existence-oracle rule). `active` means the validation loop is running, not that anything was checked. And do not copy `validUntil` from the test fixtures — they all use `1750000000`, June 2025, which ingestion rejects as already expired.

### The local demo: BYOS as the only solver

This walkthrough ends with a settlement transaction on a local anvil whose solver
is BYOS's account. It runs a full CoW stack from
[`cowdao-grants/offline-mode`](https://github.com/cowdao-grants/offline-mode) —
the real orderbook, autopilot, driver and baseline binaries built from a pinned
`cowprotocol/services` revision — with `byos` and `subsolver` as host processes.
Nothing about the driver is forked or patched, which is [ADR-0002](docs/adr/0002-solver-engine.md)'s
central claim under test: BYOS is a vanilla solver engine, and plugging it in is
configuration. What does it is a `driver.toml` and a compose override in
[`dev/offline-mode/`](dev/offline-mode), alongside the sub-solver's config for
this chain.

Beyond the development prerequisites above you need Docker, Node 18+, `jq`, and
Foundry's `cast`. Two setup steps are one-time and slow:

```sh
git submodule update --init offline-mode
git -C offline-mode submodule update --init modules/services
docker compose -f offline-mode/docker-compose.yml build \
    db-migrations orderbook autopilot driver baseline coingecko-mock
```

Init the submodules one at a time rather than with `--recursive`. offline-mode's
`modules/frontend` is the cowswap monorepo and runs to gigabytes; the demo never
starts the UIs, so only `modules/services` is needed. That build compiles the
services workspace in release mode and takes 20–60 minutes cold. **Give Docker at
least 8 GiB of memory first.** Cargo runs one rustc job per available CPU, and on
a 4 GiB VM the peak gets the `contracts` crate OOM-killed part way through.

Then bring the stack up and re-apply the chain state BYOS needs:

```sh
just stack-up
```

That is idempotent by design, and you will run it again. anvil's cheats live in
memory, so the solver whitelist, the deployed Escrow and the sub-solver's
collateral are all lost on a stack restart; none of it is baked into
offline-mode's committed state. The recipe whitelists BYOS's account in the GPv2
Authenticator, deploys the Escrow through the CREATE2 singleton factory with
`SUBMITTER_ROLE` on that account, funds the sub-solver, and deposits collateral
well above `--min-collateral`.

Now three shells:

```sh
just byos-stack        # the service
just subsolver-stack   # the reference sub-solver, polling the stack's orderbook
just stack-order       # place one order as the trader
```

`stack-order` sells 10 GNO for WETH with 3% surplus by default, which is roughly
the margin offline-mode needs before a solver will engage at all. Watch the
sub-solver discover the order, route it through the local Uniswap V2, and submit
a signed proposal; watch byos validate it to `active`; watch the autopilot cut an
auction and the driver call `/solve`. Then ask the chain who settled:

```sh
just stack-settled
```

It reads the newest GPv2 `Settlement` event and compares its solver to anvil
account 3. The chain is the referee on purpose. byos also learns the outcome, but
from the driver's `/notify` (ADR-0010), and the claim worth demonstrating is that
an independent observer sees BYOS's address on the settlement.

Roles map to anvil's standard test accounts, pinned as `just` variables so the
recipes and the two config files cannot drift apart: 0 is baseline, 1 the escrow
operator, 2 the escrow admin, 3 BYOS's settlement submitter, 4 the sub-solver, 5
the trader. BYOS gets its own account rather than sharing baseline's for two
reasons. Two settlement pipelines on one EOA can pick the same nonce when
auctions overlap, and the autopilot identifies solvers by address, so sharing one
would make "BYOS won this auction" unprovable from outside.

`just stack-up` starts baseline's container but leaves it out of the auction, so
BYOS runs unopposed. Who competes is the autopilot's `DRIVERS` variable and
nothing else: append
`,baseline|http://driver/baseline|0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` to
it in [`dev/offline-mode/compose.byos.yml`](dev/offline-mode/compose.byos.yml) to
put baseline back. Expect BYOS to lose that contest today. On a single Uniswap V2
pool the reference sub-solver finds the same route baseline does and pays extra
gas through the Trampoline, so its score comes out lower. Winning needs a
sub-solver with an edge baseline lacks, which is separate work.

#### Why byos listens on 0.0.0.0

`just byos-stack` passes `--internal-addr 0.0.0.0:9586`. The internal listener
defaults to loopback because the proposal book `/solve` returns is MEV-relevant
and only a co-deployed driver should ever see it. Here the driver is in a
container and byos is on the host, so it reaches back through
`host.docker.internal` and loopback would refuse the connection. Widening the
bind address is a local-development choice, not a deployment pattern.

Two host ports are remapped for the same class of reason, both exported by the
Justfile. offline-mode publishes the orderbook's metrics on 9586, which is byos's
internal listener, so the metrics publish moves to 9587. Its Postgres and ours
both want 5432, so its moves to 5433. Neither changes anything inside the stack,
where services still reach `db:5432`.

#### When it misbehaves

A proposal that 202s and then 404s on the read is the config mismatch described
above, and in this stack the usual cause is `--chain-id`. The fixture's anvil
runs `--chain-id 1` so that the mainnet contract addresses in its state are
self-consistent; 31337 is right for `just byos-local` and wrong here. Same for
`--trampoline-factory`, which is a real deployment here. Both are `just`
variables (`stack-chain-id`, `stack-trampoline-factory`); if you overrode one by
hand, suspect that before suspecting the service.

If `stack-up` reports that the Escrow landed at an address it did not expect,
`just sync-abis` regenerated the vendored artifact and shifted every CREATE2
address. Update `stack-escrow`, `stack-trampoline-factory` and
[`dev/offline-mode/subsolver.toml`](dev/offline-mode/subsolver.toml) to what it
printed.

An image build that fails instantly on `file not found for module alloy` means a
previous build died part way through, most likely on memory. `contracts/build.rs`
upstream generates its bindings into the source tree, while BuildKit caches
`/src/target` and re-copies the source fresh, so cargo thinks the build script
already ran and its output is gone. Clear the poisoned cache with
`docker builder prune --filter type=exec.cachemount` and build again.

See [ADR-0009](docs/adr/0009-testing-strategy.md) for the two-tier e2e design and
the chain fixture this shares. Automated tier-2 tests consume this environment;
they are not part of it.

## License

[GPL-3.0-or-later](LICENSE).
