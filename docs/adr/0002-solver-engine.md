# Solver engine

Status: proposed

Spec: docs/shared/design-document.md#solver-engine
      https://bleu.github.io/byos-docs/design-document#solver-engine

> Ported from [`bleu/cow-byos-architecture` ADR-0005](https://github.com/bleu/cow-byos-architecture/blob/main/docs/adr/0005-solver-engine.md). Still **proposed** — the open questions at the bottom are unresolved and several depend on CoW core team input. This is the ADR the `byos` crate implements; treat the open questions as the first things to settle during M2.

## Context

The BYOS engine is the **solver engine** component of the CoW driver + solver architecture. The driver handles solution encoding, gas simulation, scoring (surplus + protocol fees in native token), and settlement submission. The solver engine's job is narrower: answer the driver's `/solve` request with candidate solutions sourced from its proposal cache.

This ADR settles how BYOS selects, validates, wraps, and returns proposals as solutions — encoding the decisions made in the contract ([contracts ADR-0001](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0001-trampoline-topology.md)), escrow ([contracts ADR-0002](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0002-escrow-contract.md)), slashing ([ADR-0003](0003-slash-attribution-flow.md)), and API ([ADR-0001](0001-proposal-api.md)) ADRs into engine behavior.

Key reference: the driver's competition pipeline (`crates/driver/src/domain/competition/mod.rs` in [cowprotocol/services](https://github.com/cowprotocol/services)) — solver returns `Vec<Solution>`, driver encodes → simulates gas → scores (`surplus + protocol_fees` in native token) → ranks → re-simulates on each new block until deadline.

## Decision

The scoring formula, selection steps, settlement crafting shape, gas cut mechanics, and solution shape are specified in the design document (see Spec link above). This ADR records *why* each choice was made and the implementation-level details the spec omits.

### Selection granularity: one proposal per solution, per order UID

No batching across sub-solvers — this preserves the "one sub-solver per settlement tx" attribution rule ([ADR-0003](0003-slash-attribution-flow.md)). The driver's `SolutionMerging` is set to **`Forbidden`** to prevent the driver from blindly merging solutions from different sub-solvers into a single settlement.

Multi-order proposals are out of scope for v1. Sub-solvers propose single-order routes; CoW-finding is left to other solvers in the auction.

### Scoring: why no fee term

There is **no fee term**, and its absence is deliberate. CoW's score is surplus plus protocol fees and nothing else; gas never appears as a subtraction. It reaches the score only because a solver declares gas as its own fee, which lowers what the user receives, which lowers surplus. The protocol fee cancels out of ranking (the "ranking is fee-neutral" property in the contracts' [cow-fee-collection reference](https://github.com/bleu/byos-contracts/blob/main/docs/reference/cow-fee-collection.md)). Once our cut equals the gas cost, `surplus − gas` is the score the autopilot will compute.

**We do not estimate protocol fees either.** The driver applies them itself, then encodes and simulates before bidding. A solution that cannot absorb the fee fails that simulation and is dropped: no revert, no penalty, no escrow debit. It is also impossible before `/solve`: fee policies are built per auction by the autopilot and delivered only in the `/solve` payload.

BYOS's score remains a pre-ranking: it decides which proposals deserve the driver's encoding budget. The driver re-scores after encoding and simulation.

### Selection: single best per order UID

The RFP asks for the greatest surplus "after any configured BYOS fee", and there is no configured fee. The only one BYOS charges is the gas cut, always on and sized at cost, which is why ranking on `surplus - gas` is already surplus after the fee.

### Validation split: ingestion vs `/solve`

Heavy validation runs in the **background validation loop** (ADR-0001, ADR-0013). Cheap validation runs at **`/solve` time** (local computation, no RPC): expiry, order liveness, amount matching, scoring, and best-per-order selection.

EBBO baseline is **not** re-checked at `/solve` time. The ingestion-time check is the primary gatekeeping layer; re-running it on the hot path adds latency for marginal safety.

### Gas cut

> Supersedes a percentage-of-`sellAmount` fee, deleted rather than amended: it took the cut by routing less than the user sold, which a fixed signed route does not allow. The configurable rate, its default of 0, and the `surplus >= fee_rate × sellAmount` ingestion gate went with it.

The cut is **declared as the fulfillment's `fee`** while the route still carries the full sell amount. Using the fee field rather than shading prices keeps `encode_settle` producing the transaction we simulated, and books the cut as a declared solver fee instead of slippage. Confirmed with Haris Angelidakis (2026-07-22) that "fees are a price wedge, not a transfer."

**The limit check is ours**, because the price is ours: skip the proposal when the cut would drop the user below what they signed for. It needs no fee policies, and it can reject a proposal the score accepts — the score converts surplus at the auction's price while the limit is enforced on the route's own amounts, and a stale price makes the two disagree.

**The cut is not padded.** A bigger one lowers our score, which lowers CIP-85 consistency rewards; those come from a shared bucket allocated by closeness to the winner, so we do not recapture what we add to it.

### Solution shape: implementation details

**Every real order is limit class**, which is what the whole fee question turns on. `shared/src/order_validation.rs` assigns `Limit` when the signed `feeAmount` is zero, and every order has signed zero since the 2023 fee-model change. `Fulfillment::new` accepts `Fee::Static` (what `fee: None` becomes) only for `Market` orders, so a missing fee is rejected on every order we bid — and the DTO conversion collects into a `Result`, so one invalid trade discards **every** solution in the response.

Quote requests invert this: the driver's synthetic quote order is `Market` class unless `quote_using_limit_orders` is set. Unreachable today — the synthetic order carries the all-zero uid — but it is a trap for anyone adding quoting deliberately.

### Order amount matching

BYOS does **not** clamp or adapt proposal amounts. The sub-solver computed a route for specific amounts; changing them would invalidate the route. Scoring and gas-cut computation use `Candidate::scaled_to_fill` to scale the order's limits to the fill fraction — ceil-div for buy-side limits on sell orders (matching GPv2Settlement's rounding), floor-div for sell-side limits on buy orders (matching Solidity's default integer division).

### On-chain outcome observation: self-contained

Superseded by [ADR-0010](0010-settlement-outcome-source.md): outcomes arrive from the stock driver's notifications at `/notify`, so there is no chain watcher and no event parsing.

### `/solve` trust boundary: internal listener only

`/solve` must only be reachable by our co-deployed driver, because its response is MEV-relevant. Origin is enforced by network topology — `/solve` binds to a separate internal address (`--internal-addr`, loopback by default). As defense-in-depth, `--solve-bearer-token` optionally requires `Authorization: Bearer <token>`.

### `/solve` latency: non-issue by design

The storage half of this section is superseded by
[ADR-0013](0013-proposal-lifecycle-and-retention.md#storage-postgres-is-the-proposal-store):
the in-memory hot store is gone, and `/solve` reads the `proposals` table. One
indexed read over a few hundred live rows is about a millisecond. No simulation
and no RPC calls on the hot path.

## Open questions (not settled, flagged for discussion)

- **Batching across sub-solvers** (Q1 Option B) — could BYOS combine proposals from different sub-solvers for the same directed token pair into a batched solution? **Out of scope — will not be implemented for now**: it requires reworking the one-sub-solver-per-settlement-tx attribution rule ([ADR-0003](0003-slash-attribution-flow.md)) and the merging strategy. Potential surplus gain from batching vs attribution complexity, if ever revisited.
- **Thin Trampoline** (Q6 Option A) — **Resolved: fat Trampoline confirmed.** BYOS encodes two interactions (`sellToken.transfer` + one `execute` call); signature verification, route execution, and returning funds to the settlement stay in the contract. The contract/service split is a contracts-repo decision ([contracts ADR-0005](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0005-trampoline-execution-authority.md)), not a BYOS-service one — kept here only because the question originated in this ADR.
- **Ingestion-time profitability gate** (Q7) — **Resolved by [ADR-0013](0013-proposal-lifecycle-and-retention.md):** proposals are rejected at the first simulation when the score is not positive (`RejectionReason::Unprofitable`), matching the `/solve`-time score > 0 filter; the gate is not re-applied on re-validation.
- **Driver integration for outcome observation** (Q8 Options B/C) — **Resolved by [ADR-0010](0010-settlement-outcome-source.md):** outcome observation needs no custom driver hook — the stock driver's `/notify` protocol already delivers per-solution outcomes. No driver fork is needed for anything else either: the gas cut is solver-side work, done in the solution we return (§Gas cut).

## Alternatives considered

- **Fully delegate scoring to the driver (no BYOS-internal ranking).** BYOS would return all valid proposals and let the driver's scoring pipeline decide. Rejected — floods the driver's encoding budget with obviously worse proposals, wasting gas simulation RPC calls. BYOS's internal `surplus - gas` pre-ranking ensures only competitive proposals consume encoding slots.
- **Return all valid proposals (no pre-filter).** Rejected — the driver's encoding pipeline is the bottleneck (each solution requires gas simulation via RPC). Flooding it with obviously worse proposals wastes the encoding budget and risks hitting the deadline. A cheap ratio-based pre-filter keeps the load manageable.
- **Every-block continuous simulation.** Rejected — simulating all standing proposals every ~12s is substantial RPC load with diminishing returns. The driver's post-encoding re-simulation catches anything that goes stale between BYOS's periodic cycles. A 3–5 block interval is a practical trade-off.
- **EBBO re-check at `/solve` time.** Rejected — requires a price lookup on the hot path, adding latency. The ingestion-time baseline check is the primary defense; simulation catches routes that stopped working. The marginal safety of a fresh EBBO check doesn't justify the cost.
- **Multi-order proposals in v1.** Rejected — CoW-finding is the protocol's core competence; other solvers already do it. The entire stack (proposal schema, Trampoline `execute`, escrow debit, simulation) is designed for single-order proposals. Revisit in v2 if sub-solvers demonstrate CoW-finding ability.
- **Fee over CoW rewards (not trade amounts).** Rejected — the RFP specifies "percentage of volume or surplus," i.e. a fee extracted from the trade, not from CoW rewards. A reward-based fee also cannot gate proposals at ingestion time (rewards are not known until after settlement).
- **Revert-rate discounting (reliability oracle).** Rejected for v1 — tempting to discount surplus by historical revert probability, but the sub-solver set is small, calibration is uncertain, and escrow debits already penalize unreliable sub-solvers economically. Premature optimization.
- **Enable driver `SolutionMerging`.** Rejected — the driver merges blindly by token pair without sub-solver awareness. Would silently break the one-sub-solver-per-settlement-tx attribution rule ([ADR-0003](0003-slash-attribution-flow.md)).
- **Top-N per order UID (return 3–5 candidates).** Return multiple proposals per order and let the driver break scoring ties after encoding. Rejected — the RFP specifies "selects the one yielding the greatest surplus," and BYOS's pre-ranking is close enough to the driver's effective scoring that picking one is reliable. Sending multiple wastes encoding budget on proposals BYOS already ranked lower. The marginal fallback benefit (if the top pick fails re-simulation) does not justify the divergence from the RFP or the encoding cost.
- **Clamp proposal amounts to remaining fill.** Rejected — changing amounts invalidates the sub-solver's computed route. Instead, proposals that exceed the remaining fillable amount are silently skipped at `/solve` time (non-terminal) and sub-solvers resubmit with updated amounts via their polling loop.

## Consequences

- **BYOS is a thin layer with internal scoring.** The engine's `/solve` scores the live proposal rows and encodes two Trampoline interactions (ERC-20 transfer + execute). Scoring uses the gas the background simulation stored on each proposal. The driver still performs its own scoring after encoding — BYOS's score is a pre-ranking, not the final word.
- **Scoring divergence from the driver.** BYOS's `surplus - gas` uses cached gas estimates and reference prices, which may diverge from the driver's post-encoding gas simulation and real-time price feeds. Because BYOS sends a single proposal per order, there is no fallback if the selected proposal fails the driver's post-encoding re-simulation — BYOS loses that order for that auction round. Accepted: the divergence is marginal in practice (gas estimates are close, surplus dominates for competitive proposals), and sub-solvers naturally resubmit via their polling loop.
- **No batching means lower theoretical maximum score.** Single-order solutions can't capture CoW surplus or batching efficiencies. BYOS competes on single-order execution quality. Acceptable in v1 — the target use case is "execution against a baseline the sub-solver computed."
- **Outcome observation costs no extra infrastructure**, per [ADR-0010](0010-settlement-outcome-source.md): the driver's notifications carry the settlement result, so there is no watcher to run and no `GPv2Settlement` events to parse.
- **The profitability gate may reject viable proposals.** The ingestion-time surplus estimate uses the gas price last seen on an auction, so a proposal rejected during a gas spike would have been profitable minutes later. `Rejected` is terminal (ADR-0013), so the sub-solver has to resubmit. Accepted: the alternative is holding proposals we would not bid, and the polling loop resubmits cheaply.
- **The gas cut recovers gas approximately, not exactly.** It is sized from the auction's native price, while the weekly payout converts at an average observed over roughly an hour around the trade. Padding the cut would cost more in CIP-85 consistency rewards than it recovers, so the gap is accepted and monitored through CoW's per-solver dashboard of gas paid against gas collected.
- **Proposal freshness gap.** With 3–5 block simulation intervals, proposals can be up to ~60s stale when served at `/solve`. The driver's re-simulation catches this, but with pick-one there is no fallback if the stale proposal fails. Acceptable trade-off vs every-block RPC load; sub-solvers resubmit naturally.
