# BYOS Service

The off-chain service for **Bring Your Own Solver (BYOS)**: a bonded [CoW Protocol](https://cow.fi) solver that sources settlement routes from permissionless external sub-solvers. Sub-solvers submit EIP-712-signed routing proposals against specific order UIDs, collateralized by an on-chain escrow; BYOS scores them, feeds the best into CoW's solver competition through a standard driver, and settles winners through per-sub-solver Trampoline contracts.

Built under a [CoW DAO grant](https://forum.cow.fi/t/grant-application-cow-byos-bring-your-own-solver/3476) answering the [BYOS RFP](https://forum.cow.fi/t/rfp-bring-your-own-solver-byos/3469). The on-chain half (Escrow, Trampoline, TrampolineFactory) lives in [`bleu/byos-contracts`](https://github.com/bleu/byos-contracts).

Status: **skeleton** — structure, ADRs, and conventions are in place; implementation has not started.

## Crates

| Crate | Description | Status |
|---|---|---|
| [`byos`](crates/byos) | The BYOS service: public proposal API + CoW solver engine, one process, two listeners | skeleton |
| [`subsolver`](crates/subsolver) | Reference sub-solver: example proposal-API client, also the e2e-test counterpart | skeleton |
| [`proposal-dto`](crates/proposal-dto) | Shared wire types for the proposal API | skeleton |
| [`e2e`](crates/e2e) | End-to-end tests, two tiers: in-process against plain anvil, and full CoW stack via [offline-mode](https://github.com/cowdao-grants/offline-mode) | skeleton |

## Architecture

Sub-solvers discover orders from the public CoW orderbook, compute routes, and `POST` signed proposals to the public listener. Ingestion validates synchronously (signature recovery, escrow collateral, simulation, gatekeeping) and caches accepted proposals in memory with their scores. The CoW driver calls `/solve` on the internal listener; the engine returns the single best proposal per order UID, wrapped in one Trampoline `execute` call, and the driver competes with it as a normal solver. Background workers re-simulate standing proposals, watch `GPv2Settlement` for outcomes, and debit escrow on attributable reverts.

Start with [`CONTEXT.md`](CONTEXT.md) for the domain language, then the ADRs:

- [ADR-0001](docs/adr/0001-proposal-api.md) — proposal API & sub-solver authorization
- [ADR-0002](docs/adr/0002-solver-engine.md) — solver engine (still proposed; open questions listed inside)
- [ADR-0003](docs/adr/0003-slash-attribution-flow.md) — slashing policy & attribution
- [ADRs 0004–0009](docs/adr/README.md) — engineering conventions ported from [`cowprotocol/services`](https://github.com/cowprotocol/services)

CoW protocol background (solver auctions, slashing policy, CIPs) is captured in [`docs/reference/`](docs/reference), SLO targets in [`docs/metrics-reasoning.md`](docs/metrics-reasoning.md).

## Development

Prerequisites: stable Rust (via [rustup](https://rustup.rs); `rust-toolchain.toml` pins the channel), a nightly toolchain for rustfmt, [`just`](https://github.com/casey/just), and [`cargo-nextest`](https://nexte.st). E2e tests additionally need [Foundry](https://getfoundry.sh)'s anvil. Running the service (and the DB-backed tests) needs Postgres — `docker compose up -d postgres` provides one. In production, pass the connection string via the `DATABASE_URL` env var rather than `--database-url`: CLI arguments are visible to other users on the host via `ps`.

```sh
just build          # cargo build --workspace
just test-unit      # cargo nextest run
just test-db        # service-level tests against the compose Postgres
just test-e2e       # e2e tier 1: in-process against plain anvil
just test-e2e-full  # e2e tier 2: against a running offline-mode stack
just clippy         # -D warnings, all features and targets
just fmt            # cargo +nightly fmt (never stable fmt)
```

Full-stack e2e uses [`cowdao-grants/offline-mode`](https://github.com/cowdao-grants/offline-mode) — the real CoW orderbook/autopilot/driver on a local anvil — with BYOS plugged in as a competing solver. See [ADR-0009](docs/adr/0009-testing-strategy.md) for the two-tier design and the shared chain fixture.

## License

[LGPL-3.0-or-later](LICENSE). The `proposal-dto` crate is MIT OR Apache-2.0 so sub-solver implementations can vendor it freely.
