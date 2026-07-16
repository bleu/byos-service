# CoW Protocol — Auctions & Solver Competition (reference)

> Consolidated from the official docs under <https://docs.cow.fi/cow-protocol/reference/core/auctions>.
> Captured 2026-06-18 for offline consultation while exploring the BYOS RFP ([BYOS RFP](https://forum.cow.fi/t/rfp-bring-your-own-solver-byos/3469)). For authoritative/current text, follow the source link in each section.
>
> Why this matters for BYOS: BYOS is a bonded solver that must win the standard CoW auction. Everything below — how solutions are scored, how the fair combinatorial auction picks winners, EBBO, rewards, accounting, and bonding — applies to BYOS itself. Sub-solver proposals must ultimately produce a *valid, competitive* CoW solution under these rules.

CoW Protocol uses an implementation of the [Fair Combinatorial Auction](https://arxiv.org/abs/2408.12225) (FCA) to execute trades. A **solver** is an algorithm that takes an auction instance (valid orders, liquidity state, protocol rules/fees) and outputs one or more **solutions** selecting order subsets and feasible amounts.

---

## 1. What is solving (the problem)

Source: <https://docs.cow.fi/cow-protocol/reference/core/auctions/the-problem>

**Inputs:** orders valid for the auction, state of liquidity sources, protocol rules including fees.
**Output:** one or multiple solutions selecting a subset of orders and specifying feasible amounts for each.

**Orders are modeled as acceptance sets** (the set of trades a user will accept):

- **Sell orders** — max sell amount, buy token, limit price (worst acceptable rate). *Fill-or-kill* (all or nothing) or *partially-fillable* (any amount up to max). Surplus = extra buy tokens received vs. the limit price.
- **Buy orders** — max buy amount, limit price. Fill-or-kill or partial. Surplus = savings vs. worst-case pricing.
- **CoW AMM orders** — always valid across auctions; solver specifies buy and sell amounts, priced from the AMM's reserves (e.g. constant product).

**Protocol fees** map accepted trades to non-negative token vectors (costs charged to users). Solver fees (gas/execution) are handled separately during optimal bidding.

**A valid solution must satisfy:**
1. **Incentive compatibility** — respect order acceptance sets (limit prices).
2. **Uniform directional clearing prices (UDCP)** — identical pricing for the same token pair in the same direction.
3. **Competition rules** — the protocol-mandated principles in §2.

**Scoring:** solutions are ranked by **total surplus + protocol fees**, denominated in a common unit (native token) via external price feeds. Buy and sell orders use distinct surplus formulas based on their limit prices and asset valuations.

---

## 2. Solver competition rules

Source: <https://docs.cow.fi/cow-protocol/reference/core/auctions/competition-rules>

Rules are enforced across three layers: **smart contracts**, **off-chain protocol infrastructure**, and **governance / social consensus**.

### Smart-contract enforcement
1. **Limit price constraint** — orders cannot execute if limit prices are violated.
2. **Solver whitelisting** — only whitelisted solvers (via bonding pools, §6) can submit settlements.

### Off-chain protocol rules
- **Scoring & validity:** a valid solution must have a **positive score** and respect **UDCP** (orders on the same directed token pair get identical prices; exceptions for orders with hooks, to account for gas).
- **Fair Combinatorial Auction (winner selection):**
  - Find the highest-scoring bid for each **directed token pair** (pair + direction).
  - These best bids are **reference outcomes** (optimal execution vs. external liquidity).
  - **Batched bids** (covering multiple directed pairs) are **filtered out** if they underperform the reference outcome on *any* pair.
  - Winners are chosen from surviving batched bids plus best single-pair bids, ensuring all orders on the same directed pair belong to one winning bid.
  - Rewards follow a **second-price auction** model (see §3).
- **Settlement validity:** execution must match the winning solution (solver, score, amounts); pre-hooks before fund transfers, post-hooks after distribution; partially-fillable orders run pre-hooks once but post-hooks per fill; settlement must land before network deadlines (~3 blocks mainnet → ~40 blocks Arbitrum/BNB).
- **Buffer usage:** solvers may use settlement-contract funds for protocol/partner fee storage, network fee coverage, slippage offsets, and internal trades with "trusted" tokens marked in auction data.

### Governance / social-consensus rules
Monitored for systematic violation; penalties include denylisting or slashing.

- **EBBO** (Ethereum Best Bid and Offer) — execution must be at least as good as baseline liquidity (e.g. on mainnet: Uniswap v2/v3, Sushiswap, Swapr, Balancer v2, Pancakeswap) against base tokens (WETH, DAI, USDC, USDT, COMP, MKR, WBTC, GNO). Details in §5.
- **Prohibited behaviors:** score inflation (fake tokens / wash trading), illegal buffer usage, surplus shifting between orders sharing tokens, pennying/overbidding, hook violations. The protocol reserves discretion to slash other malicious conduct.

---

## 3. Solver rewards

Source: <https://docs.cow.fi/cow-protocol/reference/core/auctions/rewards>
Governed by CIPs 20, 27, 36, 38, 48, 57, 67, 72, 74, 85 (see [`solver-cips.md`](./solver-cips.md)). Tracking: [Dune dashboard](https://dune.com/cowprotocol/cow-solver-rewards). Rewards paid weekly in COW.

### Performance reward
```
performanceReward_i = cap( totalScore − referenceScore_i − missingScore_i )
```
- **totalScore** — sum of all winning solutions' scores in the auction.
- **referenceScore_i** — score of a counterfactual auction excluding solver *i*'s bids.
- **missingScore_i** — scores from solver *i*'s solutions that reverted.
- **cap(x) = max(−c_l, min(c_u, x))**. Upper cap `c_u` = β (chain fraction) of protocol fees earned by that solver; lower cap `c_l` is chain-specific.

| Chain | β | Lower cap c_l |
|---|---|---|
| Ethereum, Arbitrum, Base | 50% | 0.010 ETH |
| Gnosis Chain | 100% | 10 xDAI |
| Avalanche | 100% | 0.3 AVAX |
| Polygon | 100% | 30 POL |
| BNB | 100% | 0.04 BNB |
| Linea, Ink | 100% | 0.0015 ETH |
| Plasma | 100% | 30 XPL |

### Consistency reward (CIP-85)
Incentivizes consistent participation. Each auction contributes `β · protocolFee_i − performanceReward_i`, distributed proportionally to the number of executed orders for which the solver submitted a solution.

### Price-estimation (quote) rewards
For solvers that quote fill-or-kill market orders that then execute. Eligibility: fill-or-kill market order, verified quote (calldata simulation succeeds), order executed, and the proposed execution at least matches the quote and passes fairness filtering. Reward = native amount or 6 COW, whichever is less (e.g. Ethereum 0.0007 ETH, Arbitrum/Base 0.00024 ETH, Gnosis 0.15 xDAI).

### Slippage & buffers
Slippage between bidding and execution is tracked weekly: positive slippage accrues in the settlement contract; negative slippage is covered via buffer usage. Net slippage is paid to or collected from the solver.

### Strategic note
Solvers report a **cost-adjusted score** (they bear gas + revert penalties). Recommended approach: group orders by directed token pair, route optimally per group, and additionally submit batched solutions where combining pairs yields efficiencies.

---

## 4. Accounting process

Source: <https://docs.cow.fi/cow-protocol/reference/core/auctions/accounting>

- **Weekly, Tuesday→Tuesday UTC.** Auctions are bucketed into a week by block deadline; quote rewards by execution block.
- **Rewards/penalties:** successful on-time submissions earn native-token rewards; reverts/late submissions incur penalties. Dune's `capped_payment` column tracks per-auction amounts.
- **COW conversion:** performance + quote rewards denominated in COW, converted using the average COW/USD price over the final 24h of the period (manipulation resistance).
- **Protocol & partner fees:** denominated in the order's surplus token, converted to native token via auction prices. Protocol fees → CoW DAO; partner fees → designated recipient. DAO amount = `protocol_fee − partner_fee`.
- **Buffer accounting:** protocol/partner fees collected in the settlement contract, network fees (sell-token, converted to native and paid weekly), and per-tx slippage (raw imbalance minus expected fees, converted via price feed). Network fee derived from the difference between actual amounts and amounts implied by the fee-free UCP vector.
- **Payout adjustments:** **service fee** on positive COW rewards (default 15%, per CIP-48); **minimum transfer thresholds**; **overdraft handling** via the overdrafts manager contract `0x8fd67ea651329fd142d7cfd8e90406f133f26e8a` (`solverOverdraftBalance`, `payOverdraft`); curated **auction price corrections**.

---

## 5. EBBO violations

Source: <https://docs.cow.fi/cow-protocol/reference/core/auctions/ebbo-rules> — framework from **CIP-52**.

- **Certificate of violation:** a reference routing on a block (and log index) between auction start and on-chain settlement, using only base liquidity + base tokens, establishes a baseline surplus. Violation magnitude = reference surplus − actual user surplus.
- **Challenge:** accused solver has 72h to propose an alternative block/index; the core team may set a new (final) certificate.
- **Reimbursement:** detected by core team or third parties; must be reported within 3 months. Solver gets a reimbursement demand in the surplus token and 72h to comply. Compliance closes the case.
- **Escalation/slashing:** non-compliance → auto deny-listing, forum statement, 3-day review, then a Snapshot vote. Successful CIP → bond slashed by the refund amount, proceeds to affected users. Bond replenishment allows reinstatement (DAO may replenish from treasury).

---

## 6. Bonding pools

Source: <https://docs.cow.fi/cow-protocol/reference/core/auctions/bonding-pools>

- **Standard pool (CIP-7):** deploy a Mainnet Gnosis Safe with the CoW DAO safe as sole signer; once confirmed, fund with **$500,000** in yield-bearing stablecoins + **1,500,000 COW**. The pool can then vouch for solvers.
- **Reduced pool (CIP-44):** available to already-vouched solvers; grants full control over calldata and on-chain submission. Requires core-team approval. Lower requirements: **$50,000** stablecoins/ETH + **500,000 COW** initially, scaling to **$100,000** + **1,000,000 COW** over the following year. Still formally vouched under the CoW DAO pool.
- **Vouching:** call `Vouch` on the `VouchRegister` contracts (multi-chain) with the pool owner's signature; registers submission address, pool address, and rewards address in one tx.
- **Exit/dissolution:** `invalidateVouching` to leave; to dissolve, unvouch all solvers, post on the forum (≥6 days), then submit a CIP Snapshot proposal with tx simulations.

> **BYOS relevance:** BYOS must be a bonded solver. The RFP places the bonding capital out of scope, but BYOS will operate under a bonding pool (standard or reduced) and is liable for slashing — making the safety guarantees around sub-solver execution (Trampoline + escrow) directly load-bearing.
