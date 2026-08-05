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

### Scoring: surplus minus gas

BYOS computes its own score for each proposal: `score = surplus - gas`, in native-token units.

- **Surplus** — the improvement beyond the order's limit price: extra buy tokens on a sell order, sell tokens kept back on a buy order. Converted to native token with the auction's reference price for whichever token it lands in.
- **Gas** — the `eth_estimateGas` result of the full-settle simulation ([ADR-0012](0012-simulation.md)) plus a 30k buffer, cached on the proposal, times the auction's effective gas price.

There is **no fee term**, and its absence is deliberate. CoW's score is surplus plus protocol fees and nothing else (CIP-38, `driver/src/domain/competition/solution/scoring.rs`); gas never appears as a subtraction there. It reaches the score only because a solver declares gas as its own fee, which lowers what the user receives, which lowers surplus.

The protocol fee then cancels out of any ranking. It is carved out of surplus and added straight back, so substituting through the on-chain arithmetic leaves `score = route surplus − our own cut`, for all three policy kinds. That is the "ranking is fee-neutral" property in the contracts' [cow-fee-collection reference](https://github.com/bleu/byos-contracts/blob/main/docs/reference/cow-fee-collection.md). Once our cut equals the gas cost (§Gas cut), `surplus − gas` is the score the autopilot will compute for our bid, up to the route's price improvement over the auction's reference ratio. The cut is a fixed number of sell-token atoms, and the surplus it displaces is worth exactly that only when the route trades at the reference rate; a route beating it by 1% overstates our own score by 1% of the gas bill. That is small enough to ignore, and reading per-order fee policies would still buy nothing.

**We do not estimate protocol fees either.** The driver applies them itself (`Solution::new` calls `with_protocol_fees`, gated on `fee_handler`, default `Driver`), then encodes and simulates before bidding. A solution that cannot absorb the fee fails that simulation and is dropped: we lose the round, with no revert, no penalty and no escrow debit. Estimating the fee ourselves would only produce a slightly earlier "no". It is also impossible before `/solve`: fee policies are built per auction by the autopilot and delivered only in the `/solve` payload, so the orderbook's order model carries no fee-policy field and the ingestion gate has nothing to read. That gate checks gas headroom and nothing more. It could also size the cut and check the signed limit — that needs one extra price lookup per proposal per tick, nothing else — but it does not: ingestion stays cheap, and `/solve` skips such a proposal when it comes to bid.

BYOS's score remains a pre-ranking: it decides which proposals deserve the driver's encoding budget. The driver re-scores after encoding and simulation.

### Selection: single best per order UID

Before returning solutions, BYOS filters and ranks proposals, selecting **one winner per order UID**:

1. **Expiry** — `valid_until > now`
2. **Order liveness** — order UID is present in the auction's order list
3. **Amount matching** — proposal amounts satisfy the order's limit price and remaining fillable amount (see §Order amount matching below)
4. ~~**Escrow re-check**~~ — moved off this path: the background validator re-checks escrow and rejects (ADR-0013's transition table). `/solve` reads no chain state.
5. **Score rank** — rank by `surplus - gas` (using the gas cached at simulation and the auction's prices)
6. **Gas cut** — size the cut and drop the proposal if taking it would breach the user's signed limit (§Gas cut)
7. **Select best** — take the single highest-scoring proposal per order UID

A winner with non-positive score is not returned: settling a trade expected to cost more in gas than it earns in surplus is worse than skipping the order.

This satisfies the RFP's selection requirement, with one wrinkle: the RFP asks for the greatest surplus "after any configured BYOS fee", and there is no configured fee. The only one BYOS charges is the gas cut, always on and sized at cost (§Gas cut), which is why ranking on `surplus - gas` is already surplus after the fee. BYOS's score is a pre-ranking approximation of the driver's effective scoring; the driver still performs its own scoring after encoding.

### Validation split: ingestion vs `/solve`

Heavy validation runs in the **background validation loop**, not on the `POST /proposals` request — ingestion verifies the signature and stores the proposal as `submitted` (ADR-0001's async-ingestion revision, ADR-0013). The checks themselves are unchanged:

- EIP-712 signature recovery and verification
- Escrow balance >= minimum (cached with short TTL)
- Simulation against reference block (permanent drop on failure); gas estimate cached per proposal
- Hook presence — required pre/post hooks from order app data present in `interactions`
- Baseline price sanity — proposal not obviously worse than reference AMM prices (EBBO baseline)
- Profitability gate — proposal's surplus must exceed its gas cost (`surplus - gas > min_score`, ADR-0013). This gate cannot see fee policies: they exist only in the `/solve` payload. It asks about gas headroom and deliberately stops there (§Scoring)
- Rate limiting (IP-based + signer-based, escrow-tiered)

Cheap validation runs at **`/solve` time** (local computation, no RPC):

- Expiry, order liveness, amount matching against the auction's order state, scoring + best-per-order selection with the non-positive-score drop (as above). The escrow re-check belongs to the validation loop, not this path (ADR-0013).

EBBO baseline is **not** re-checked at `/solve` time. The ingestion-time check is the primary gatekeeping layer. Proposals that passed EBBO at ingestion and still simulate successfully at settlement carry low EBBO risk. Re-running it on every `/solve` adds latency for marginal safety.

### Continuous simulation: BYOS-level, periodic

BYOS re-simulates standing proposals against the current block state on a **configurable interval** (`--validation-interval-secs`, default 12s — about one block; ADR-0001 records the same correction). Proposals that revert are permanently dropped ([ADR-0001](0001-proposal-api.md) lifecycle rule). Sub-solvers resubmit via their polling loop.

This is **not** every-block simulation — the RPC load of simulating all standing proposals every 12s is substantial and unnecessary. The driver's post-encoding re-simulation (`resimulate_until_revert`) catches proposals that go stale between BYOS simulation cycles.

### Settlement crafting: two interactions per proposal

BYOS wraps each proposal in **two** intra-settlement interactions:

1. **`sellToken.transfer(trampoline, sellAmount)`** — BYOS-owned. Pushes trade capital from the `GPv2Settlement` contract to the sub-solver's Trampoline instance. The Trampoline cannot access Settlement funds directly, so this transfer is mandatory and always encoded by BYOS.
2. **`trampoline.execute(proposal, interactions, sellToken, buyToken, signature)`** — runs the sub-solver's signed route inside the Trampoline sandbox. Everything that happens inside that call — signature verification, route execution, returning funds to the settlement, and the funding guard that enforces the signed amounts — is contract behavior, specified by [contracts ADR-0003](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0003-trampoline-deployment-settlement-integration.md) and [ADR-0005](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0005-trampoline-execution-authority.md). The token addresses are BYOS-supplied call parameters taken from the order, not signed proposal fields.

The engine's responsibilities: compute the Trampoline CREATE2 address from the sub-solver's address (recovered from the proposal signature), ABI-encode the ERC-20 transfer, and ABI-encode the `execute` call. This is pure local computation (keccak256 + ABI encoding) — no RPC on the `/solve` hot path.

The Solution returned to the driver contains these two `Interaction::Custom` entries targeting the sell token and the Trampoline respectively. The driver sees two interactions per order.

### Gas cut

> Supersedes a percentage-of-`sellAmount` fee, deleted rather than amended: it took the cut by routing less than the user sold, which a fixed signed route does not allow. The configurable rate, its default of 0, and the `surplus >= fee_rate × sellAmount` ingestion gate went with it.

BYOS charges exactly the gas the settlement is estimated to cost, in sell-token units, always on — no multiplier, no config knob. It is **declared as the fulfillment's `fee`** while the route still carries the full sell amount. Declaring and routing less are separable, and only the first is open to us: the sub-solver signed for the full `proposal.sell_amount`. The wedge lands anyway, because the driver rebuilds the clearing prices from our declared execution, so the user receives `R × (1 - f/S)` and the difference stays in the `GPv2Settlement` buffers. Nothing reimburses gas; what returns weekly is money we declined to pass on ([contracts ADR-0003](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0003-trampoline-deployment-settlement-integration.md): "fees are a price wedge, not a transfer" — confirmed with Haris Angelidakis, 2026-07-22). Using the field rather than shading prices ourselves keeps `encode_settle` producing the transaction we simulated, and books the cut as a declared solver fee instead of slippage.

**The limit check is ours**, because the price is ours: skip the proposal when the cut would drop the user below what they signed for. It needs no fee policies, and it can reject a proposal the score accepts — the score converts surplus at the auction's price while the limit is enforced on the route's own amounts, and a stale price makes the two disagree.

**The cut is not padded.** A bigger one lowers our score, which lowers CIP-85 consistency rewards; those come from a shared bucket allocated by closeness to the winner, so we do not recapture what we add to it. Revenue margin above gas recovery is deliberately left open here. BYOS retains 100% of CoW rewards earned under its bonded solver address; pass-through to sub-solvers is out of scope for v1.

### Solution shape: what we put in the `Solution` we return

Every field, and why:

| Field | Value | Why |
| --- | --- | --- |
| `id` | index within this response, 1-based | Recorded against the proposal id so `/notify` can be attributed (ADR-0013) |
| `prices` | `{sell_token: proposal.buy_amount, buy_token: proposal.sell_amount}` | Cross-multiplied from the proposal amounts. Unaffected by the cut, which is a declared fee rather than a price shade |
| `trades` | exactly one `Fulfillment` | One order per solution (§Selection granularity) |
| `trades[0].fee` | the gas cut, in sell-token atoms | **Never `None`.** Every live order is limit class, so the driver requires a solver-determined fee and rejects `Fee::Static` |
| `trades[0].executed_amount` | sell order: `candidate.order_sell - fee`. Buy order: `candidate.order_buy` | For fill-or-kill, `candidate.order_sell` equals the signed order amount. For partial fills, it equals the proposal's fill amount (`Candidate::scaled_to_fill` scales order limits to the fill fraction). The driver requires `executed + fee == target` for sell orders and leaves the fee out of that check for buy orders |
| `interactions` | two `Custom` entries, `internalize: false` | The transfer and the Trampoline `execute` (§Settlement crafting) |
| `pre_interactions`, `post_interactions`, `wrappers` | empty | Hook-carrying orders are outside the simulation envelope (ADR-0012) |
| `gas` | simulated gas + 30k buffer | The driver's settlement budget; the same number the cut is priced from |
| `flashloans` | `None` | Out of scope for v1 |

**Every real order is limit class**, which is what the whole fee question turns on. `shared/src/order_validation.rs` assigns `Limit` when the signed `feeAmount` is zero, and every order has signed zero since the 2023 fee-model change. `Fulfillment::new` accepts `Fee::Static` (what `fee: None` becomes) only for `Market` orders, so a missing fee is rejected on every order we bid — and the DTO conversion collects into a `Result`, so one invalid trade discards **every** solution in the response.

Quote requests invert this: the driver's synthetic quote order is `Market` class unless `quote_using_limit_orders` is set, so there a solver-determined fee is what gets rejected. Unreachable today — the synthetic order carries the all-zero uid, which can never have an `Active` proposal, and a quote auction prices no tokens so the cut cannot be sized — but it is a trap for anyone adding quoting deliberately.

### Order amount matching

At `/solve` time, BYOS validates proposal amounts against the auction's order state:

- **Fill-or-kill orders** — proposal amounts must exactly match the order's target amount (sell amount for sell orders, buy amount for buy orders). Mismatches are rejected.
- **Partially fillable orders** — the proposal may fill any fraction of the order. A pre-fill guard skips proposals whose fill exceeds the remaining auction amount (which shrinks as other solvers fill the order across auctions). This is a non-terminal filter: the proposal stays `Active` and may be selected in a future auction where the remaining amount is larger (e.g. the order was not filled as much as expected). Scoring and gas-cut computation use `Candidate::scaled_to_fill` to scale the order's limits to the fill fraction — ceil-div for buy-side limits on sell orders (matching GPv2Settlement's rounding), floor-div for sell-side limits on buy orders (matching Solidity's default integer division) — so `score_proposal` and `gas_cut::size` work unchanged for both order types.

BYOS does **not** clamp or adapt proposal amounts. The sub-solver computed a route for specific amounts; changing them would invalidate the route. Sub-solvers resubmit with updated amounts via their polling loop when order state changes.

### On-chain outcome observation: self-contained

BYOS monitors the chain directly for settlement outcomes. It watches `GPv2Settlement` events, matches settlements to proposals via the Trampoline CREATE2 address in calldata ([contracts ADR-0001](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0001-trampoline-topology.md)), and triggers Track A escrow debits on revert ([ADR-0003](0003-slash-attribution-flow.md)). Before debiting, BYOS classifies the revert: failures caused by its own infrastructure — e.g. a trampoline missing after a deposit-tx reorg ([contracts ADR-0003](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0003-trampoline-deployment-settlement-integration.md)) — are BYOS's cost, not the sub-solver's. Only reverts attributable to the sub-solver's route trigger a debit.

Superseded by [ADR-0010](0010-settlement-outcome-source.md): outcomes arrive from the stock driver's notifications at `/notify`, so there is no chain watcher and no event parsing. The driver still treats BYOS as a vanilla solver engine.

### `/solve` trust boundary: internal listener only

`/solve` and the public proposal API have opposite trust boundaries. The proposal API must be internet-reachable so sub-solvers can submit; `/solve` must only be reachable by our co-deployed driver, because its response is the full standing proposal book for an auction — sub-solver amounts, routes, and signatures, all MEV-relevant. The two endpoint groups therefore never share a socket: `/solve` (plus `/healthz`) binds to a separate internal address (`--internal-addr`, loopback by default), and origin is enforced by network topology rather than path obscurity (COW-1174).

As defense-in-depth, `--solve-bearer-token` optionally requires `Authorization: Bearer <token>` on `/solve`; the driver sends it via its `[solver.request-headers]` config. The token complements the listener split, it does not replace it.

### `/solve` latency: non-issue by design

The storage half of this section is superseded by
[ADR-0013](0013-proposal-lifecycle-and-retention.md#storage-postgres-is-the-proposal-store):
the in-memory hot store is gone, and `/solve` reads the `proposals` table. The
latency argument survives the change — one indexed read over a few hundred live
rows is about a millisecond — and the rest of the path is unchanged:

1. Receive auction, deserialize orders — fast
2. An indexed read of the live proposal rows per auction order — ~1ms each
3. Pre-filter: expiry, liveness, scoring — microseconds
4. Encode two interactions (ERC-20 transfer + Trampoline execute) — keccak256 + ABI encoding, sub-millisecond per proposal
5. One `solutions` insert per returned bid (ADR-0013: if we cannot record it, we do not bid it)
6. Return `Vec<Solution>`

No simulation and no RPC calls on the hot path. The expensive work happens in
the background validation loop (simulation, escrow check). The design meets any
reasonable `/solve` SLO.

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
