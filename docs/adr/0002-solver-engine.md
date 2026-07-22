# Solver engine

Status: proposed

> Ported from [`bleu/cow-byos-architecture` ADR-0005](https://github.com/bleu/cow-byos-architecture/blob/main/docs/adr/0005-solver-engine.md). Still **proposed** — the open questions at the bottom are unresolved and several depend on CoW core team input. This is the ADR the `byos` crate implements; treat the open questions as the first things to settle during M2.

## Context

The BYOS engine is the **solver engine** component of the CoW driver + solver architecture. The driver handles solution encoding, gas simulation, scoring (surplus + protocol fees in native token), and settlement submission. The solver engine's job is narrower: answer the driver's `/solve` request with candidate solutions sourced from its proposal cache.

This ADR settles how BYOS selects, validates, wraps, and returns proposals as solutions — encoding the decisions made in the contract ([contracts ADR-0001](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0001-trampoline-topology.md)), escrow ([contracts ADR-0002](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0002-escrow-contract.md)), slashing ([ADR-0003](0003-slash-attribution-flow.md)), and API ([ADR-0001](0001-proposal-api.md)) ADRs into engine behavior.

Key reference: the driver's competition pipeline (`crates/driver/src/domain/competition/mod.rs` in [cowprotocol/services](https://github.com/cowprotocol/services)) — solver returns `Vec<Solution>`, driver encodes → simulates gas → scores (`surplus + protocol_fees` in native token) → ranks → re-simulates on each new block until deadline.

## Decision

### Selection granularity: one proposal per solution, per order UID

BYOS returns **one solution per selected proposal**, each covering a **single order UID**. The driver receives them as independent single-order solutions and scores them independently. The autopilot's FCA handles ranking across all solvers. Concretely: an auction containing N orders for which BYOS holds valid proposals yields up to N independent single-order solutions, and the combinatorial auction can award several of them in the same round — each settling as its own transaction. Solutions are never grouped by token pair.

No batching across sub-solvers — this preserves the "one sub-solver per settlement tx" attribution rule ([ADR-0003](0003-slash-attribution-flow.md)). The driver's `SolutionMerging` is set to **`Forbidden`** to prevent the driver from blindly merging solutions from different sub-solvers into a single settlement.

Multi-order proposals (one proposal covering multiple order UIDs) are out of scope for v1. Sub-solvers propose single-order routes; CoW-finding is left to other solvers in the auction.

### Scoring: BYOS-internal, mirroring the driver

BYOS computes its own score for each proposal: `score = surplus + fee - gas`.

- **Surplus** — the buy-side improvement beyond the order's limit price **net of the BYOS fee** (what the user actually receives), converted to native token using cached reference prices.
- **Fee** — the BYOS fee for the settlement (see §Fee mechanism below), converted to native token using the same reference prices.
- **Gas** — estimated gas cost from `eth_estimateGas` simulation plus a 100k buffer, cached on the proposal.

Because surplus is net of fee, `surplus + fee` is the gross price improvement — the fee is not counted twice, and ranking follows total value created rather than the fee portion.

This mirrors the driver's scoring approach (`surplus + protocol_fees` in native token, `scoring.rs`) so that BYOS's ranking closely tracks the driver's final ranking. The driver still performs its own scoring after encoding and gas simulation — BYOS's score is a pre-ranking to select which proposals deserve the driver's encoding budget.

### Selection: single best per order UID

Before returning solutions, BYOS filters and ranks proposals, selecting **one winner per order UID**:

1. **Expiry** — `valid_until > now`
2. **Order liveness** — order UID is present in the auction's order list
3. **Amount matching** — proposal amounts satisfy the order's limit price and remaining fillable amount (see §Order amount matching below)
4. **Escrow re-check** — sub-solver's cached escrow balance >= minimum (`gas + c_l`)
5. **Score rank** — rank by `surplus + fee - gas` (using cached values from ingestion)
6. **Select best** — take the single highest-scoring proposal per order UID

A winner with non-positive score is not returned: settling a trade expected to cost more in gas than it earns in surplus and fee is worse than skipping the order.

This matches the RFP requirement: "the engine selects the one yielding the greatest surplus after any configured BYOS fee." BYOS's score is a pre-ranking approximation of the driver's effective scoring; the driver still performs its own scoring after encoding.

### Validation split: ingestion vs `/solve`

Heavy validation runs at **proposal ingestion** (`POST /proposals`), per [ADR-0001](0001-proposal-api.md):

- EIP-712 signature recovery and verification
- Escrow balance >= minimum (cached with short TTL)
- Simulation against reference block (permanent drop on failure); gas estimate cached per proposal
- Hook presence — required pre/post hooks from order app data present in `interactions`
- Baseline price sanity — proposal not obviously worse than reference AMM prices (EBBO baseline)
- Fee gate — proposal's surplus must cover BYOS fee (`surplus >= fee_rate × sellAmount`, both sides in native token — see §Fee mechanism)
- Rate limiting (IP-based + signer-based, escrow-tiered)

Cheap validation runs at **`/solve` time** (in-memory, no RPC):

- Expiry, order liveness, amount matching against the auction's order state, escrow re-check, scoring + best-per-order selection with the non-positive-score drop (as above)

EBBO baseline is **not** re-checked at `/solve` time. The ingestion-time check is the primary gatekeeping layer. Proposals that passed EBBO at ingestion and still simulate successfully at settlement carry low EBBO risk. Re-running it on every `/solve` adds latency for marginal safety.

### Continuous simulation: BYOS-level, periodic

BYOS re-simulates standing proposals against the current block state on a **configurable interval** (default: every 3–5 blocks, ~36–60s on mainnet). Proposals that revert are permanently dropped ([ADR-0001](0001-proposal-api.md) lifecycle rule). Sub-solvers resubmit via their polling loop.

This is **not** every-block simulation — the RPC load of simulating all standing proposals every 12s is substantial and unnecessary. The driver's post-encoding re-simulation (`resimulate_until_revert`) catches proposals that go stale between BYOS simulation cycles.

### Settlement crafting: two interactions per proposal

BYOS wraps each proposal in **two** intra-settlement interactions:

1. **`sellToken.transfer(trampoline, sellAmount)`** — BYOS-owned. Pushes trade capital from the `GPv2Settlement` contract to the sub-solver's Trampoline instance. The Trampoline cannot access Settlement funds directly, so this transfer is mandatory and always encoded by BYOS.
2. **`trampoline.execute(proposal, interactions, buyToken, signature)`** — runs the sub-solver's signed route inside the Trampoline sandbox. Everything that happens inside that call — signature verification, route execution, the exact-amount settle-back that acts as the funding guard — is contract behavior, specified by [contracts ADR-0003](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0003-trampoline-deployment-settlement-integration.md) and [ADR-0005](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0005-trampoline-execution-authority.md).

The engine's responsibilities: compute the Trampoline CREATE2 address from the sub-solver's address (recovered from the proposal signature), ABI-encode the ERC-20 transfer, and ABI-encode the `execute` call. This is pure local computation (keccak256 + ABI encoding) — no RPC on the `/solve` hot path.

The Solution returned to the driver contains these two `Interaction::Custom` entries targeting the sell token and the Trampoline respectively. The driver sees two interactions per order.

### Fee mechanism: percentage of sellAmount

BYOS charges a configurable fee on winning solutions, expressed as a **percentage of `sellAmount`** (RFP §Economics: "percentage of volume or surplus"). The fee rate **defaults to 0** in v1.

At ingestion, proposals whose surplus does not cover the fee are rejected: `surplus < fee_rate × sellAmount → reject`. The comparison is done in native token — `fee_rate × sellAmount` is converted from sell-token units using the same cached reference prices as surplus. This matches the RFP requirement that "proposals whose surplus does not cover the fee are rejected at acceptance."

At settlement, the fee is extracted from the trade's surplus — the user receives at least the order's limit price, BYOS retains the fee portion, and any remaining surplus above the fee flows to `GPv2Settlement` as positive slippage. BYOS also retains 100% of CoW rewards earned under its bonded solver address; reward pass-through to sub-solvers is out of scope for v1.

### Order amount matching: strict, no clamping

At `/solve` time, BYOS validates proposal amounts against the auction's order state:

- **Fill-or-kill orders** — proposal amounts must satisfy the order's limit price. Mismatches are rejected.
- **Partially fillable orders** — proposal's `sell_amount` must be <= remaining fillable amount. If it exceeds (order was partially filled since proposal submission), the proposal is rejected.

BYOS does **not** clamp or adapt proposal amounts. The sub-solver computed a route for specific amounts; changing them would invalidate the route. Sub-solvers resubmit with updated amounts via their polling loop when order state changes.

### On-chain outcome observation: self-contained

BYOS monitors the chain directly for settlement outcomes. It watches `GPv2Settlement` events, matches settlements to proposals via the Trampoline CREATE2 address in calldata ([contracts ADR-0001](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0001-trampoline-topology.md)), and triggers Track A escrow debits on revert ([ADR-0003](0003-slash-attribution-flow.md)). Before debiting, BYOS classifies the revert: failures caused by its own infrastructure — e.g. a trampoline missing after a deposit-tx reorg ([contracts ADR-0003](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0003-trampoline-deployment-settlement-integration.md)) — are BYOS's cost, not the sub-solver's. Only reverts attributable to the sub-solver's route trigger a debit.

This keeps BYOS self-contained — no driver modifications required. The driver treats BYOS as a vanilla solver engine, interacting only via `/solve`. BYOS needs chain awareness anyway for continuous simulation; the settlement watcher piggybacks on the same infrastructure.

### `/solve` latency: non-issue by design

The entire `/solve` hot path is served from an in-memory cache with local computation only:

1. Receive auction, deserialize orders — fast
2. Scan proposal cache per order UID — O(proposals per order), in-memory
3. Pre-filter: expiry, liveness, escrow re-check (cached), scoring — microseconds
4. Encode two interactions (ERC-20 transfer + Trampoline execute) — keccak256 + ABI encoding, sub-millisecond per proposal
5. Return `Vec<Solution>`

No simulation, no RPC calls, no database queries on the hot path. All expensive work happens at ingestion (simulation, signature verification, escrow check) or during continuous simulation (periodic re-simulation). The design naturally meets any reasonable `/solve` SLO.

## Open questions (not settled, flagged for discussion)

- **Batching across sub-solvers** (Q1 Option B) — could BYOS combine proposals from different sub-solvers for the same directed token pair into a batched solution? **Deferred to v2**: it requires reworking the one-sub-solver-per-settlement-tx attribution rule ([ADR-0003](0003-slash-attribution-flow.md)) and the merging strategy, so it cannot land in v1. Potential surplus gain from batching vs attribution complexity.
- **Thin Trampoline** (Q6 Option A) — BYOS encodes every step (approvals, sub-solver calls, sweep, transfer out) as separate interactions instead of delegating them to `execute`. The `sellToken.transfer` is already BYOS-owned in both approaches; the difference is whether the remaining steps (signature verification, route execution, settle-back) live in the contract or in BYOS-encoded interactions. More driver control, more encoding complexity, safety logic moves from contract to BYOS.
- **Ingestion-time profitability gate** (Q7) — the `/solve`-time score > 0 filter already prevents settling trades that are unprofitable on cached estimates; the open question is whether proposals should additionally be rejected at `POST` time, sparing sub-solvers a standing proposal that will never be selected.
- **Driver integration for outcome observation** (Q8 Options B/C) — if the CoW team allows driver modifications, BYOS could receive settlement outcome callbacks from the driver instead of running its own chain watcher. Reduces infrastructure, but creates coupling. Pending CoW team response.

## Alternatives considered

- **Fully delegate scoring to the driver (no BYOS-internal ranking).** BYOS would return all valid proposals and let the driver's scoring pipeline decide. Rejected — floods the driver's encoding budget with obviously worse proposals, wasting gas simulation RPC calls. BYOS's internal `surplus + fee - gas` pre-ranking ensures only competitive proposals consume encoding slots.
- **Return all valid proposals (no pre-filter).** Rejected — the driver's encoding pipeline is the bottleneck (each solution requires gas simulation via RPC). Flooding it with obviously worse proposals wastes the encoding budget and risks hitting the deadline. A cheap ratio-based pre-filter keeps the load manageable.
- **Every-block continuous simulation.** Rejected — simulating all standing proposals every ~12s is substantial RPC load with diminishing returns. The driver's post-encoding re-simulation catches anything that goes stale between BYOS's periodic cycles. A 3–5 block interval is a practical trade-off.
- **EBBO re-check at `/solve` time.** Rejected — requires a price lookup on the hot path, adding latency. The ingestion-time baseline check is the primary defense; simulation catches routes that stopped working. The marginal safety of a fresh EBBO check doesn't justify the cost.
- **Multi-order proposals in v1.** Rejected — CoW-finding is the protocol's core competence; other solvers already do it. The entire stack (proposal schema, Trampoline `execute`, escrow debit, simulation) is designed for single-order proposals. Revisit in v2 if sub-solvers demonstrate CoW-finding ability.
- **Fee over CoW rewards (not trade amounts).** Rejected — the RFP specifies "percentage of volume or surplus," i.e. a fee extracted from the trade, not from CoW rewards. A reward-based fee also cannot gate proposals at ingestion time (rewards are not known until after settlement).
- **Revert-rate discounting (reliability oracle).** Rejected for v1 — tempting to discount surplus by historical revert probability, but the sub-solver set is small, calibration is uncertain, and escrow debits already penalize unreliable sub-solvers economically. Premature optimization.
- **Enable driver `SolutionMerging`.** Rejected — the driver merges blindly by token pair without sub-solver awareness. Would silently break the one-sub-solver-per-settlement-tx attribution rule ([ADR-0003](0003-slash-attribution-flow.md)).
- **Top-N per order UID (return 3–5 candidates).** Return multiple proposals per order and let the driver break scoring ties after encoding. Rejected — the RFP specifies "selects the one yielding the greatest surplus," and BYOS's pre-ranking is close enough to the driver's effective scoring that picking one is reliable. Sending multiple wastes encoding budget on proposals BYOS already ranked lower. The marginal fallback benefit (if the top pick fails re-simulation) does not justify the divergence from the RFP or the encoding cost.
- **Clamp proposal amounts to remaining fill.** Rejected — changing amounts invalidates the sub-solver's computed route. Sub-solvers resubmit with updated amounts via their polling loop.

## Consequences

- **BYOS is a thin layer with internal scoring.** The engine's `/solve` serves scored proposals from an in-memory cache with two-interaction Trampoline encoding (ERC-20 transfer + execute). Scoring uses cached values from ingestion-time simulation (surplus, gas estimate). The driver still performs its own scoring after encoding — BYOS's score is a pre-ranking, not the final word.
- **Scoring divergence from the driver.** BYOS's `surplus + fee - gas` uses cached gas estimates and reference prices, which may diverge from the driver's post-encoding gas simulation and real-time price feeds. Because BYOS sends a single proposal per order, there is no fallback if the selected proposal fails the driver's post-encoding re-simulation — BYOS loses that order for that auction round. Accepted: the divergence is marginal in practice (gas estimates are close, surplus dominates for competitive proposals), and sub-solvers naturally resubmit via their polling loop.
- **No batching means lower theoretical maximum score.** Single-order solutions can't capture CoW surplus or batching efficiencies. BYOS competes on single-order execution quality. Acceptable in v1 — the target use case is "execution against a baseline the sub-solver computed."
- **Self-contained chain monitoring is additional infrastructure.** BYOS must run a settlement watcher, parse `GPv2Settlement` events, and map Trampoline addresses to sub-solvers. This piggybacks on the block-subscription infra needed for continuous simulation, but is still operational surface area.
- **Fee gate may reject viable proposals.** The ingestion-time surplus estimate and fee calculation could reject proposals that would ultimately be profitable (gas estimate may be high, surplus may improve). Mitigated by setting a conservative fee rate (0 in v1) and tuning as real data accumulates.
- **Proposal freshness gap.** With 3–5 block simulation intervals, proposals can be up to ~60s stale when served at `/solve`. The driver's re-simulation catches this, but with pick-one there is no fallback if the stale proposal fails. Acceptable trade-off vs every-block RPC load; sub-solvers resubmit naturally.
