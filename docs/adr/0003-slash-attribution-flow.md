# Slashing policy & attribution flow

Status: accepted

Spec: docs/shared/design-document.md#penalties
      https://bleu.github.io/byos-docs/design-document#penalties

> Ported from [`bleu/cow-byos-architecture` ADR-0003](https://github.com/bleu/cow-byos-architecture/blob/main/docs/adr/0003-slash-attribution-flow.md). This is the canonical, service-side version of the policy; [`bleu/byos-contracts` ADR-0004](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0004-penalty-schedule-and-attribution.md) is a contract-scoped extract of the same decision. The escrow mechanics referenced here (debit, freeze, events) are implemented as an ERC20 ledger per [contracts ADR-0007](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0007-erc20-escrow-token.md); use `effectiveBalance()` for eligibility checks.

## Context

When CoW imposes a cost on BYOS, BYOS must attribute it to the responsible sub-solver and recover it from escrow ([contracts ADR-0002](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0002-escrow-contract.md)) — without being able to fabricate a slash against an honest sub-solver. Two coupled sub-decisions from the [economics design note](https://github.com/bleu/cow-byos-architecture/blob/main/docs/design/byos-subsolver-economics.md):

- **Q3** — Track-B arbiter: who adjudicates EBBO/fairness passthrough claims?
- **Q4** — Attribution: how does BYOS map a settlement to the responsible sub-solver?

CoW's own penalty framework has four enforcement layers (see [`../shared/reference/cow-solver-slashing-policy.md`](../shared/reference/cow-solver-slashing-policy.md)):

1. **Smart contract** (limit price reverts, whitelist) — architecturally prevented by the Trampoline ([contracts ADR-0001](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0001-trampoline-topology.md)); sub-solvers never call `settle`.
2. **Automated off-chain** (participation guards, banning) — subsumed by BYOS's gatekeeping + Track A escrow debits + collateral gate. No separate replication needed.
3. **DAO governance** (EBBO, score inflation, surplus shifts, overbidding, hooks, catch-all) — only **EBBO/unfair pricing** and the **catch-all** apply to sub-solvers. Score inflation, illegal buffer usage, surplus shifting, and overbidding are either architecturally prevented or are BYOS's own responsibility (BYOS controls score construction, buffer access, and settlement composition).
4. **Economic penalties** (reward formula, `missingScore`, `c_l` cap) — mirrored via the Track A `gas + c_l` debit. Sub-solvers receive no rewards in v1, so the escrow debit is the only lever.

### Hooks and sub-solver responsibility

Hooks are handled by BYOS and the driver, not by sub-solvers (COW-1243). The orderbook pre-encodes hooks as `HooksTrampoline.execute()` calls; BYOS includes them in simulation for accurate gas estimates, and the driver appends them to the settlement. Sub-solvers submit routing proposals without hook interactions — their EIP-712 signature covers the route, not the hooks. Some hooks (e.g., withdrawing DEX liquidity before a swap) change the token balances available for the route; sub-solvers must account for their effects when computing routes, but do not encode or submit them.

## Decision

The penalty schedule, Track A and Track B flows, attribution rule, `c_l` values, minimum escrow, shortfall policy, gatekeeping description, and transparency model are specified in the design document (see Spec link above). This ADR records *why* each choice was made.

### Track A is BYOS-unilateral

For reverts and deadline misses, everything is on-chain-verifiable (tx receipt, gas cost, Trampoline CREATE2 address → sub-solver attribution per [contracts ADR-0001](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0001-trampoline-topology.md)). The non-settlement sub-category is based on BYOS's internal auction records and is not independently verifiable by the sub-solver — this is an accepted trust assumption.

**Non-settlement detection** (decided with COW-1205): a driver `Cancelled`/`Expired`/`Fail` notification for an `Executing` proposal is the trigger — the driver confirmed it began submitting (`SettlementStarted`, [ADR-0010](0010-settlement-outcome-source.md)) and then abandoned the settlement with no tx landing. The executing-*timeout* release ([ADR-0013](0013-proposal-lifecycle-and-retention.md)) is **not** charged: a lost notification is not proof of non-settlement.

### Track B arbiter: CoW core team

They already adjudicate EBBO; routing Track B to them ensures BYOS cannot fabricate a certificate. Sub-solvers receive the same evidence standard, challenge window, and appeal rights that CoW gives BYOS.

### Attribution: one sub-solver per settlement tx

The Trampoline CREATE2 address in calldata self-evidences which sub-solver's route ran — no reliance on BYOS's private records. Cost: less batching efficiency. Accepted — clean attribution is worth more than marginal gas savings.

### Minimum escrow: low barrier by design

Sized to cover worst-case Track A (`gas + c_l`). Track B is inherently under-collateralized; gatekeeping is the primary Track B defense, not escrow sizing.

### Shortfall: no bans, no debt tracking

Bans are meaningless in a permissionless system; the escrow loss is the penalty. The sub-solver is naturally suspended (zero collateral = ineligible).

### Gatekeeping is non-exculpatory

Passing gatekeeping does not absolve the sub-solver. This is the core framing: gatekeeping reduces risk, it does not eliminate it.

### Policy lifecycle

Immutable for v1. No unilateral updates. Changes require a v2 policy with a new escrow deployment or migration.

## Alternatives considered

- **Replicate all four CoW enforcement layers.** Rejected — Layers 1 and 2 are either architecturally prevented or already covered by gatekeeping + escrow + collateral gate. Adding separate participation guards (ban timers) would be redundant.
- **Pass all Layer 3 violations through to sub-solvers** (score inflation, buffer abuse, surplus shifting, overbidding). Rejected — sub-solvers cannot cause most of these violations. BYOS controls score construction, buffer access, and settlement composition. Listing violations sub-solvers can't commit would be confusing.
- **Sub-solvers encode hooks in their proposals.** Reversed (COW-1243) — BYOS now handles hooks: the orderbook pre-encodes them, BYOS includes them in simulation, and the driver appends them to the settlement. Sub-solvers provide only routing interactions. Hooks that change token state (e.g., LP withdrawal before swap) affect the route but are not the sub-solver's responsibility to encode.
- **No penalty for non-settlement** (BYOS wins auction but doesn't settle). Rejected — non-settlement degrades BYOS's participation-guard standing with CoW. Sub-solvers should internalize this cost.
- **Credit Track A against Track B for the same settlement.** Rejected — if both penalties hit BYOS, the sub-solver should pay both. The sub-solver's proposal caused both problems.
- **Formal dispute mechanism for Track A with external arbiter.** Rejected — Track A is on-chain-verifiable. BYOS-unilateral adjudication is sufficient given the trust model. Sub-solvers trust BYOS as operator.
- **Public slashing dashboard / reporting.** Rejected — leaks competitive intelligence about sub-solver routing quality. On-chain escrow events are sufficient for the sub-solver to audit their own history.
- **Permanent ban or debt tracking on escrow shortfall.** Rejected — meaningless in a permissionless system. New address = new identity. The escrow loss itself is the penalty.
- **Higher minimum escrow (proportional to order value, or fixed large amount).** Rejected — Track B is inherently under-collateralized regardless of minimum size. Low barriers to entry matter for a permissionless system. `gas + c_l` covers the common case (Track A).
- **Versioned or updatable policy.** Rejected for v1 — adds complexity. Ship, learn, revisit in v2.

## Consequences

- **Sub-solvers trust BYOS for Track A adjudication.** BYOS is both debitor and dispute judge. Mitigation: Track A parameters are on-chain-verifiable; a provably incorrect debit is an operational bug, not a policy failure. All debits emit events with reasons.
- **Track B has an unrecoverable gap.** If the sub-solver withdrew or escrow < claim, BYOS absorbs the shortfall. This is why gatekeeping is mandatory — it is the primary Track B defense.
- **Non-settlement penalty (10% of `c_l`) relies on BYOS's internal records.** Sub-solvers cannot independently verify that BYOS won an auction with their proposal. Accepted trust assumption, consistent with the operator trust model.
- **One sub-solver per settlement tx reduces batching efficiency.** Accepted — clean attribution enables indisputable Track A debits and clean Track B passthrough.
- **Immutable v1 policy means no ability to adjust parameters.** If 10% of `c_l` for non-settlement proves too low or too high, it stays until v2. Mitigated by conservative initial sizing and the expectation that v1 is a learning phase.
- **36h sub-solver challenge window for Track B is tight.** Sub-solvers need responsive operations to gather evidence within 36h. Permissionless participants without 24/7 ops may struggle. Accepted — BYOS needs the remaining 36h of its 72h CoW window to process and relay.
- **The service tracks a 5× off-chain reserve against pending Track-B claims** ([contracts ADR-0002](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0002-escrow-contract.md)): no on-chain reserve multiplier exists, so reserve calculation and proposal-eligibility math are a BYOS-service responsibility and a critical path.
