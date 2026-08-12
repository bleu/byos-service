# BYOS Service — Project Context

This repo holds the off-chain half of the **Bring Your Own Solver (BYOS)** design: the **BYOS service** (`crates/byos` — proposal API, solver engine, gatekeeping, monitoring, escrow operations) and the **reference sub-solver** (`crates/subsolver`). The on-chain half — Escrow, Trampoline, TrampolineFactory — lives in [`bleu/byos-contracts`](https://github.com/bleu/byos-contracts) and is out of scope here except as an integration surface. Source RFP: [Bring Your Own Solver (BYOS)](https://forum.cow.fi/t/rfp-bring-your-own-solver-byos/3469) · [accepted grant application](https://forum.cow.fi/t/grant-application-cow-byos-bring-your-own-solver/3476).

Domain vocabulary and the normative specification are in `docs/shared/`. See `docs/shared/glossary.md` for terms and `docs/shared/design-document.md` for the spec.

## Components (RFP scope)

1. **Solver engine** (`crates/byos`) — answers the standard CoW driver's `/solve` from the proposal store; internal `surplus - gas` pre-ranking; single best proposal per order UID; fat-Trampoline settlement crafting ([ADR-0002](docs/adr/0002-solver-engine.md)).
2. **Proposal API** (`crates/byos`) — public HTTP, EIP-712-signed, **permissionless but collateral-gated**; `POST`/`GET`(metadata only)/`DELETE`; two-layer rate limiting ([ADR-0001](docs/adr/0001-proposal-api.md)). Specified in [`crates/byos/openapi.yml`](crates/byos/openapi.yml).
3. **Background workers** (`crates/byos`) — re-simulation and expiry, Track A debits, the retention sweep, escrow-balance cache refresh, off-chain Track-B reserve tracking.
4. **Reference sub-solver** (`crates/subsolver`) — example client and e2e-test counterpart.
5. Plus: operational runbook + monitoring ([ADR-0008](docs/adr/0008-observability.md)).

Process topology: **one process, two listeners** — a public port for `/proposals` and a firewalled internal port for `/solve` and `/notify`, sharing the Postgres proposal store ([ADR-0001](docs/adr/0001-proposal-api.md), [ADR-0013](docs/adr/0013-proposal-lifecycle-and-retention.md)).

v1 targets **Ethereum mainnet + Gnosis**. Out of scope: BYOS-operated orderbook, reward pass-through to sub-solvers, cross-chain escrow accounting, BYOS's own bonding capital.

## Service design posture

- BYOS requires **no changes to the CoW auction/competition** — it is a black box to the protocol, a vanilla solver engine to the driver.
- Simulation failures cost the sub-solver **nothing** (rate-limit only); only on-chain failures debit escrow.
- The API is **permissionless + collateral-gated**, not allowlisted — the escrow deposit *is* the permission.
- The escrow contract is a dumb ledger; the service is the brain — reserve calculations, proposal eligibility, gatekeeping, attribution, and dispute handling all live here.
- The `/solve` hot path does no simulation and no RPC — an indexed read of the live proposal rows per auction order, plus one `solutions` insert per returned bid ([ADR-0013](docs/adr/0013-proposal-lifecycle-and-retention.md) revisited ADR-0001's no-DB rule).
