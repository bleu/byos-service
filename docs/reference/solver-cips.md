# CoW DAO CIPs — Solver Competition (reference index)

> CoW Improvement Proposals (CIPs) that **establish, modify, or discuss** the solver competition: who can solve, how solutions are scored and ranked, how winners are chosen, rewards, fees, bonding, and enforcement/slashing.
> Captured 2026-06-18 for the BYOS exploration ([BYOS RFP](https://forum.cow.fi/t/rfp-bring-your-own-solver-byos/3469)). Status/details summarized from forum search — open each link for authoritative text. The consolidated mechanics live in [`solver-auctions.md`](./solver-auctions.md).

## Auction & winner-selection mechanism

| CIP | Title | What it changes / discusses |
|---|---|---|
| **CIP-67** | [Moving from batch auction to the fair combinatorial auction](https://forum.cow.fi/t/cip-67-moving-from-batch-auction-to-the-fair-combinatorial-auction/2967) | The current core mechanism. Replaces single-winner batch auction with the FCA: per-directed-pair reference bids, filtering of underperforming batched bids, multiple winners. Higher throughput + stronger per-pair fairness guarantees. **Most relevant to how BYOS bids win.** |
| **CIP-11** | [Rules of the Solver Competition — status quo and update](https://forum.cow.fi/t/cip-11-rules-of-the-solver-competition-status-quo-and-an-update-proposal/1016) | Foundational competition rules: social-consensus (implicit) rules, global + local token-conservation constraints. |
| **CIP-13** | [Rules update: ban pennying](https://forum.cow.fi/t/cip-13-rules-of-the-solver-competition-update-proposal-to-ban-pennying/1119) | Prohibits pennying (deliberately inflating reported scores expecting rewards to cover losses). |
| **CIP-38** | [Solver Computed Fees & Rank by Surplus](https://forum.cow.fi/t/cip-38-solver-computed-fees-rank-by-surplus/2061) | Solvers compute their own "network fee" to cover gas; ranking moves to surplus-based. |
| **CIP-72** | [Aligning quoting and solving behavior of solvers](https://forum.cow.fi/t/cip-72-aligning-quoting-and-solving-behavior-of-solvers/3079) | Addresses solvers giving over-optimistic quotes but not matching bids at solve time. **Relevant to BYOS quote/solve consistency.** |

## Rewards & fees

| CIP | Title | What it changes / discusses |
|---|---|---|
| **CIP-20** | [Auction model for solver rewards](https://forum.cow.fi/t/cip-20-auction-model-for-solver-rewards/1405) | Establishes the auction-based (second-price) COW reward model for the competition winner. |
| **CIP-34** | [Testing Fee Models for CoW Protocol](https://forum.cow.fi/t/cip-34-testing-fee-models-for-cow-protocol/1984) | Early experimentation with protocol fee models. |
| **CIP-36** | [Adjusting and renewing solver rewards budget](https://forum.cow.fi/t/cip-36-adjusting-and-renewing-solver-rewards-budget/2244) | Renews the rewards budget (committed 8M COW for the competition). |
| **CIP-48** | [Solver rewards budget renewal & update of bonding pool operations](https://forum.cow.fi/t/cip-48-solver-rewards-budget-renewal-and-update-of-cow-dao-bonding-pool-operations/2493) | Budget renewal + introduces the **15% service fee** on positive COW rewards for designated/bonding-pool solvers. |
| **CIP-57** | [Solver rewards on all chains](https://forum.cow.fi/t/cip-57-solver-rewards-on-all-chains/2634) | Extends rewards across all operating chains (mainnet, Gnosis, Arbitrum, …). |
| **CIP-74** | [Align Solver Rewards with Protocol Revenue + volume-based fee](https://forum.cow.fi/t/cip-74-align-solver-rewards-with-protocol-revenue-and-introduce-a-volume-based-fee/3234) | Replaces fixed reward cap with a **dynamic cap tied to protocol fees** of the winning solution; adds a 2 bps unconditional volume fee. **Directly shapes BYOS economics** (the RFP's fee defaults to 0). See also the [retrospective](https://forum.cow.fi/t/cip-74-retrospective-aligning-rewards-with-revenue/3358) and the [small-order second-price issue](https://forum.cow.fi/t/second-price-auction-is-broken-for-small-orders-since-cip-74/3317). |
| **CIP-85** | [Performance and Consistency Rewards](https://forum.cow.fi/t/cip-85-performance-and-consistency-rewards/3377) | Current reward shape: fixes reward budget at 50% of protocol revenue; adds **consistency rewards** for reliable participation. |

### Related drafts
- [CIP-Draft: Align Solver Rewards with Protocol Revenue](https://forum.cow.fi/t/cip-draft-align-solver-rewards-with-protocol-revenue/3174) — hybrid model (solvers 25% of batch surplus when the protocol collects fees; users 50%, protocol 25%). Predecessor discussion to CIP-74.
- [CIP-Draft: Distributing COW rewards on mainnet for all chains](https://forum.cow.fi/t/cip-draft-distributing-cow-rewards-on-mainnet-for-all-chains/3042)

## Bonding, eligibility & enforcement

| CIP | Title | What it changes / discusses |
|---|---|---|
| **CIP-7** | [Allowing External Solvers](https://forum.cow.fi/t/cip-7-allowing-external-solvers/923) | Establishes bonding pools ($500k + 1.5M COW Safe owned by CoW DAO); pool creators signal liability to allow-list solver addresses. **The mechanism BYOS itself is bonded under.** |
| **CIP-44** | [Reduced bonding requirements](https://forum.cow.fi/t/cip-44-reduced-bonding-requirements/2424) | Reduced bonding pool ($50k + 500k COW, scaling up) granting full calldata/submission control; still vouched under the DAO pool. **A likely path for BYOS.** |
| **CIP-52** | [EBBO (fairness) specs, reimbursement & escalation](https://forum.cow.fi/t/cip-52-ebbo-fairness-specifications-reimbursement-procedures-and-escalation-mechanisms/2579) | Defines EBBO violation certificates, reimbursement, and slashing escalation. **Defines BYOS's liability surface.** |
| **CIP-55** | [Slashing of the GlueX solver](https://forum.cow.fi/t/cip-55-slashing-of-the-gluex-solver/2649) | Concrete precedent: a solver slashed for misbehavior — illustrates real enforcement. |
| **CIP-78** | [Dissolve Sprinter Bonding Pool](https://forum.cow.fi/t/cip-78-dissolve-sprinter-bonding-pool/3241) | Example of the pool-dissolution process in practice. |
| **CIP-Draft** | [Simplifying the operations of the CoW DAO bonding pool](https://forum.cow.fi/t/cip-draft-simplifying-the-operations-of-the-cow-dao-bonding-pool/3455) | Ongoing discussion on bonding-pool operations. |

## Most load-bearing for BYOS

1. **CIP-67** — the FCA mechanism BYOS must win under.
2. **CIP-85** + **CIP-74** — current reward/fee economics (the RFP says BYOS keeps 100% of rewards, fee defaults to 0).
3. **CIP-7 / CIP-44** — bonding pool BYOS operates under.
4. **CIP-52** — EBBO/slashing, i.e. BYOS's liability if a sub-solver's proposal harms users.
5. **CIP-72** — quote/solve consistency, relevant to proposal re-simulation.

> Note: CIP numbers/status evolve. Re-check the forum's [Governance](https://forum.cow.fi/c/governance) and Closed Proposals categories for anything newer than the capture date.
