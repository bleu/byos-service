# Architecture Decision Records

One file per crystallized decision: `NNNN-short-slug.md`, numbered from `0001`. ADRs are **append-only** — when a decision changes, write a new ADR that supersedes the old one rather than editing history. Each ADR states: **context**, the **decision**, **alternatives considered**, and **consequences**. Terminology comes from [`CONTEXT.md`](../../CONTEXT.md).

**ADRs must never contradict each other.** When a new ADR supersedes part of an older one, the same change annotates the superseded section in place — a blockquote or inline *"Superseded by [ADR-NNNN]"* note pointing at the replacement (see ADR-0001's GET-metadata and persistence sections for the pattern). A reader landing on any ADR must either get current guidance or an explicit pointer to it; append-only protects history, the annotations protect the present. The one exception to append-only: an ADR still in `proposed` status may be rewritten in place, with a note recording the rewrite.

This repo owns the service-scoped ADRs for BYOS. ADRs 0001–0003 were designed during the grant proposal in [`bleu/cow-byos-architecture`](https://github.com/bleu/cow-byos-architecture) and are ported here with updated cross-links (each carries a provenance note). The contract-scoped ADRs live in [`bleu/byos-contracts`](https://github.com/bleu/byos-contracts/tree/main/docs/adr) and are referenced from here, never restated — anything about contract internals (schemas, typehashes, settlement mechanics, escrow accounting) is owned there. The one deliberate overlap: ADR-0003 here is the canonical slashing policy, and contracts ADR-0004 is its contract-scoped extract. ADRs 0004–0009 port the engineering patterns of [`cowprotocol/services`](https://github.com/cowprotocol/services), the CoW backend this service integrates with.

## Decisions

| ADR | Decision | Status |
|-----|----------|--------|
| [0001](0001-proposal-api.md) | Proposal API & sub-solver authorization | accepted |
| [0002](0002-solver-engine.md) | Solver engine (selection, scoring, settlement crafting) | **proposed** |
| [0003](0003-slash-attribution-flow.md) | Slashing policy & attribution flow | accepted |
| [0004](0004-cargo-workspace-and-tooling.md) | Cargo workspace & tooling (from cowprotocol/services) | accepted |
| [0005](0005-crate-anatomy-and-layering.md) | Crate anatomy & internal layering (from cowprotocol/services) | accepted |
| [0006](0006-configuration-and-cli.md) | Configuration & CLI (from cowprotocol/services) | accepted |
| [0007](0007-error-handling.md) | Error handling (from cowprotocol/services) | accepted |
| [0008](0008-observability.md) | Observability (from cowprotocol/services) | accepted |
| [0009](0009-testing-strategy.md) | Testing strategy (from cowprotocol/services) | accepted |
| [0010](0010-settlement-outcome-source.md) | Settlement outcome source & Track A trigger | accepted |
| [0011](0011-owner-scoped-reads.md) | Signature-gated, owner-scoped proposal reads | accepted |
| [0012](0012-simulation.md) | Proposal simulation via `eth_estimateGas` | accepted |
| [0013](0013-proposal-lifecycle-and-retention.md) | Proposal lifecycle & retention | accepted |

## Known open questions

- **ADR-0002 is still proposed.** Cross-sub-solver batching is out of scope for now, and the fat Trampoline is confirmed (the contract/service split is owned by the contracts repo). The ingestion-time profitability gate is settled by [ADR-0013](0013-proposal-lifecycle-and-retention.md) (reject at first simulation, `Unprofitable`). The "driver callbacks for outcome observation" question is resolved by [ADR-0010](0010-settlement-outcome-source.md).
- **Anvil-state regeneration procedure** — the e2e chain fixture bakes the BYOS contracts into offline-mode's `anvil-state.json` ([ADR-0009](0009-testing-strategy.md)); the exact regeneration workflow (and whether the deploy hook lands upstream in offline-mode) is settled when the e2e crate gains its first test.
- **Reference-price source for scoring** — [ADR-0002](0002-solver-engine.md) assumes cached native-token reference prices for surplus/fee conversion; where they come from (auction payload, external feed) is unspecified.
