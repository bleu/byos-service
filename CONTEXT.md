# BYOS Service — Project Context

The stable domain language for the **Bring Your Own Solver (BYOS)** project, scoped to its off-chain service. Read this before exploring; use its vocabulary in issues, ADRs, and code. Source RFP: [Bring Your Own Solver (BYOS)](https://forum.cow.fi/t/rfp-bring-your-own-solver-byos/3469) · [accepted grant application](https://forum.cow.fi/t/grant-application-cow-byos-bring-your-own-solver/3476). CoW protocol background: [`docs/reference/`](docs/reference).

## What BYOS is

A **bonded CoW solver** whose proposed solutions are sourced from a permissionless set of **external sub-solvers**. Sub-solvers submit signed routing proposals against specific order UIDs, collateralized by an escrow balance held by BYOS. BYOS retains exclusive control over on-chain settlement submission. From the protocol's perspective BYOS is a single, ordinary bonded solver — the sub-solver relationship is entirely internal to BYOS.

This repo holds the off-chain half of that design: the **BYOS service** (`crates/byos` — proposal API, solver engine, gatekeeping, monitoring, escrow operations) and the **reference sub-solver** (`crates/subsolver`). The on-chain half — Escrow, Trampoline, TrampolineFactory — lives in [`bleu/byos-contracts`](https://github.com/bleu/byos-contracts) and is out of scope here except as an integration surface.

## Glossary

- **Sub-solver** — an external, permissionless party that computes a route for a specific order and submits a signed **proposal** to BYOS. Never holds submission keys; never calls settle. Identified by its address (recovered from its EIP-712 signature); that same address is its escrow key and its Trampoline CREATE2 salt.
- **Proposal** — an EIP-712-signed message `{order_uid, sell_amount, buy_amount, interactions, valid_until, nonce, signature}` authorizing BYOS to attempt a settlement of those interactions and consenting to the associated escrow risk. The signer address is the escrow key — there is no separate `escrow_account` field ([ADR-0001](docs/adr/0001-proposal-api.md)). Expires at `valid_until`, on settlement, on simulation failure, when the order is otherwise filled/cancelled, or on signed `DELETE`.
- **BYOS engine** — the **solver engine** half of a CoW driver + solver pair (the driver is a standard CoW driver, unmodified, with `SolutionMerging::Forbidden`). Scores proposals internally using `score = surplus + fee - gas` and answers the driver's `/solve` with the single highest-scoring proposal per order UID from the in-memory proposal store, each wrapped in one Trampoline `execute` call ([ADR-0002](docs/adr/0002-solver-engine.md)).
- **Ingestion** — the synchronous `POST /proposals` pipeline: IP filter → parse + `ecrecover` → signer rate limit → cached escrow check → gatekeeping + simulation → store with cached score. Answers with the proposal id or a machine-readable 4xx ([ADR-0001](docs/adr/0001-proposal-api.md)).
- **Proposal store** — the in-memory hot store serving `/solve` with no RPC or DB on the auction-critical path; rebuilt from fresh submissions on restart. Distinct from the **audit trail**.
- **Audit trail** — the async write-behind persistence of every proposal (≥3-month retention) used as dispute evidence for Track B claims. Operational logs are not the audit trail.
- **Gatekeeping** — BYOS's *preventive* control: validating each proposal (simulation, hook presence, EBBO baseline price) before settling. Distinct from escrow, which is *recovery*. Best-effort and non-exculpatory — passing gatekeeping does not absolve a sub-solver ([ADR-0003](docs/adr/0003-slash-attribution-flow.md)).
- **Continuous simulation** — the background loop re-simulating standing proposals every 3–5 blocks; reverting proposals are permanently dropped and sub-solvers resubmit via their polling loops.
- **Settlement watcher** — the background observer of `GPv2Settlement` events that maps settlements to sub-solvers via the Trampoline CREATE2 address in calldata, classifies reverts (sub-solver route vs BYOS infra failure), and triggers Track A debits.
- **Trampoline** — a contract that receives `sellAmount`, executes the sub-solver's interactions, returns `buyAmount` to `GPv2Settlement`, and holds no protocol balance outside a single settlement. One immutable instance per sub-solver at a deterministic CREATE2 address, deployed at escrow-deposit time. Implemented in the contracts repo ([contracts ADR-0001](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0001-trampoline-topology.md), [ADR-0003](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0003-trampoline-deployment-settlement-integration.md)).
- **Escrow** — a per-chain, native-token ERC20-ledger contract holding sub-solver collateral keyed by sub-solver address ([contracts ADR-0002](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0002-escrow-contract.md), [ADR-0007](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0007-erc20-escrow-token.md)). The service reads `effectiveBalance()` for eligibility and calls the operator functions. The collateral-at-risk is the *only* sub-solver capital BYOS touches — trade capital flows atomically through `GPv2Settlement → Trampoline`.
- **Owner** — the secure wallet (multisig/Safe) that owns the Escrow: receives debited funds, sets the operator, configures the cooldown.
- **Operator** — the EOA this service holds for automated escrow operations: `debit`, `freeze`, `unfreeze`, `pause`, `unpause`. Cannot withdraw funds or change configuration; a compromised operator can grief but not steal.
- **Debit (Track A)** — routine, provable recovery of `gas + c_l` from escrow when a winning settlement carrying a proposal reverts on-chain ([ADR-0003](docs/adr/0003-slash-attribution-flow.md)).
- **Slash / clawback (Track B)** — rare passthrough of a CoW EBBO/fairness penalty (CIP-52) to the responsible sub-solver's escrow, mirroring the process CoW runs against BYOS. The service tracks a 5× off-chain **reserve** against pending Track-B claims.
- **Freeze** — operator blocks withdrawal execution (and ERC20 transfers) for a specific sub-solver while a Track B investigation is open. Does not affect effective balance.
- **Attribution** — mapping a settlement tx back to the sub-solver whose proposal it contained. Enforced by settling **one sub-solver per settlement tx**; the per-sub-solver Trampoline CREATE2 address in calldata self-evidences which sub-solver's route ran ([ADR-0003](docs/adr/0003-slash-attribution-flow.md)).
- **`c_l`** — CoW's per-auction lower reward cap = the max revert penalty (0.010 ETH mainnet, 10 xDAI Gnosis). BYOS's debit per reverted auction is bounded by `gas + c_l`. See [`docs/reference/cow-solver-slashing-policy.md`](docs/reference/cow-solver-slashing-policy.md).

## Components (RFP scope)

1. **Solver engine** (`crates/byos`) — answers the standard CoW driver's `/solve` from the proposal store; internal `surplus + fee - gas` pre-ranking; single best proposal per order UID; fat-Trampoline settlement crafting ([ADR-0002](docs/adr/0002-solver-engine.md)).
2. **Proposal API** (`crates/byos`) — public HTTP, EIP-712-signed, **permissionless but collateral-gated**; `POST`/`GET`(metadata only)/`DELETE`; two-layer rate limiting ([ADR-0001](docs/adr/0001-proposal-api.md)).
3. **Background workers** (`crates/byos`) — continuous simulation, settlement watcher + Track A debits, escrow-balance cache refresh, off-chain Track-B reserve tracking.
4. **Reference sub-solver** (`crates/subsolver`) — example client and e2e-test counterpart.
5. Plus: operational runbook + monitoring ([ADR-0008](docs/adr/0008-observability.md)).

Process topology: **one process, two listeners** — a public port for `/proposals` and a firewalled internal port for `/solve`, sharing the in-memory proposal store ([ADR-0001](docs/adr/0001-proposal-api.md)).

v1 targets **Ethereum mainnet + Gnosis**. Out of scope: BYOS-operated orderbook, reward pass-through to sub-solvers, cross-chain escrow accounting, BYOS's own bonding capital.

## Two risk classes (the core economic framing)

| | Track A — gas + revert penalty | Track B — EBBO / fairness slash |
|---|---|---|
| Determined by | On-chain fact (tx reverted) | Off-chain CIP-52 certificate + DAO |
| Timing | Seconds → ~1 accounting week | Days → up to 3 months |
| Attributable cleanly? | Yes (tx → proposal) | Murky; BYOS *chose* to settle it |
| Recoverable from escrow? | Yes | Only if funds still present; else BYOS eats it |
| Primary defense | Escrow debit | BYOS pre-settlement **gatekeeping** |

## Service design posture

- BYOS requires **no changes to the CoW auction/competition** — it is a black box to the protocol, a vanilla solver engine to the driver.
- Simulation failures cost the sub-solver **nothing** (rate-limit only); only on-chain failures debit escrow.
- The API is **permissionless + collateral-gated**, not allowlisted — the escrow deposit *is* the permission.
- The escrow contract is a dumb ledger; the service is the brain — reserve calculations, proposal eligibility, gatekeeping, attribution, and dispute handling all live here.
- The `/solve` hot path is in-memory only: no simulation, no RPC, no DB. SLO targets and their reasoning: [`docs/metrics-reasoning.md`](docs/metrics-reasoning.md).

## Related repositories

- [`bleu/byos-contracts`](https://github.com/bleu/byos-contracts) — Escrow, Trampoline, TrampolineFactory (Foundry). The EIP-712 domain, `ProposalData` schema/typehash, and all contract interfaces are defined there; this service consumes them and must match them exactly (test against contract-provided vectors, don't re-derive).
- [`bleu/cow-byos-architecture`](https://github.com/bleu/cow-byos-architecture) — proposal-phase design repo; origin of ADRs 0001–0003 and the economics design note.
- [`cowprotocol/services`](https://github.com/cowprotocol/services) — the CoW backend (driver/autopilot) BYOS integrates with, and the source of this repo's engineering patterns (ADRs 0004–0009). The driver-facing `/solve` API is specified in its `crates/solvers/openapi.yml`; the exact revision we target is pinned via offline-mode's submodule ([ADR-0009](docs/adr/0009-testing-strategy.md)).
- [`cowdao-grants/offline-mode`](https://github.com/cowdao-grants/offline-mode) — bleu's offline CoW-stack environment (real orderbook/autopilot/driver/baseline on a local anvil with mainnet-address contracts). The full-stack e2e harness: BYOS plugs in as a competing solver via the driver's solver config ([ADR-0009](docs/adr/0009-testing-strategy.md)).
