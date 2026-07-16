# Agent Guidelines

Instructions for AI agents working on this codebase.

## Project overview

This repo contains the Rust service for BYOS (Bring Your Own Solver), a CoW Protocol solver that sources routes from permissionless external sub-solvers. The main crate is `byos` — proposal API + solver engine in one process. The `subsolver` crate is the reference sub-solver used as documentation and as the e2e-test counterpart.

## Repo structure

```
CONTEXT.md            Domain language and architecture map — read first
crates/byos/          The BYOS service (proposal API, solver engine, workers)
crates/subsolver/     Reference sub-solver client
crates/proposal-dto/  Shared wire types for the proposal API
crates/e2e/           End-to-end tests (contracts on anvil + byos + subsolver)
docs/adr/             Architecture decision records
docs/reference/       CoW protocol background (slashing, auctions, CIPs)
docs/agents/          Agent workflow conventions (issue tracker, triage labels)
docs/metrics-reasoning.md  SLO targets and cost/revenue reasoning
Justfile              The command surface — same recipes locally and in CI
```

## Before working

- Read [`CONTEXT.md`](CONTEXT.md), then the ADRs in [`docs/adr/`](docs/adr/) that touch the area you're about to work in. ADRs 0001–0003 are the domain decisions; 0004–0009 are the engineering conventions (ported from cowprotocol/services).
- If your output contradicts an existing ADR, surface it explicitly rather than silently overriding: _"Contradicts ADR-0001 (GET returns metadata only) — but worth reopening because…"_ Note that [ADR-0002](docs/adr/0002-solver-engine.md) is still **proposed**; its open questions are fair game.
- Contract interfaces this service integrates with (Escrow operator functions, Trampoline `execute`, the EIP-712 `ProposalData` schema) are owned by [`bleu/byos-contracts`](https://github.com/bleu/byos-contracts) — check there before assuming a signature or event shape.
- Issues and PRDs live as local markdown under `.scratch/` — see [`docs/agents/issue-tracker.md`](docs/agents/issue-tracker.md).

## Key conventions

- Commands run through **just**: `just build`, `just test-unit`, `just clippy`, `just fmt`. CI runs the same recipes.
- **Formatting requires nightly rustfmt** (`cargo +nightly fmt` via `just fmt`) — never stable fmt; `rustfmt.toml` uses unstable options. Format only as the final step.
- **Clippy warnings are errors** (`-D warnings`, `--all-features --all-targets`).
- All dependencies are declared once in the workspace `Cargo.toml` (`[workspace.dependencies]`); crates use `{ workspace = true }`.
- Tests run with **cargo-nextest**. Expensive tests are `#[ignore]`d and name-filtered, not feature-gated ([ADR-0009](docs/adr/0009-testing-strategy.md)).
- New service code uses **thiserror**; anyhow only in `run.rs`/test setup ([ADR-0007](docs/adr/0007-error-handling.md)).
- Wire types: camelCase JSON, 256-bit amounts as decimal strings, addresses/order UIDs as hex strings ([ADR-0005](docs/adr/0005-crate-anatomy-and-layering.md)).

## Domain language

The glossary lives in [`CONTEXT.md`](CONTEXT.md) — sub-solver, proposal, ingestion, proposal store, audit trail, gatekeeping, attribution, Track A/B, `c_l`, operator. Use those terms exactly in issue titles, test names, metric names, and code; don't drift to synonyms (it's `sub_solver`, never plain `solver`, for the external party — `solver` means BYOS itself in CoW's vocabulary). If a concept you need isn't in the glossary, that's a signal — either you're inventing language the project doesn't use (reconsider) or there's a real gap (flag it).

## Design principles

- `domain/` is pure (no IO); `infra/` owns axum, RPC, config, persistence; DTO conversion happens at the edges ([ADR-0005](docs/adr/0005-crate-anatomy-and-layering.md)).
- The `/solve` hot path never does RPC, simulation, or DB work — expensive validation belongs in ingestion or the background loops ([ADR-0002](docs/adr/0002-solver-engine.md)).
- Rejection reasons returned to sub-solvers are typed enums, machine-readable on the wire ([ADR-0007](docs/adr/0007-error-handling.md)).
- The service is the brain, the contracts are a dumb ledger: eligibility math, reserve tracking, attribution, and dispute handling live here and deserve tests accordingly.
- Money-moving paths (debits, freezes) must be observable: metrics + events for every action ([ADR-0008](docs/adr/0008-observability.md)).
