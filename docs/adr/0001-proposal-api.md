# Proposal API & sub-solver authorization

Status: accepted

Spec: docs/shared/design-document.md#proposal-api
      https://bleu.github.io/byos-docs/design-document#proposal-api

> Ported from [`bleu/cow-byos-architecture` ADR-0004](https://github.com/bleu/cow-byos-architecture/blob/main/docs/adr/0004-proposal-api.md), where it was accepted during the grant proposal. The original ADR also settled the contract-side halves of this decision — the signature-gated `execute`, the EIP-712 `ProposalData` schema, and the factory-anchored domain. Those are owned and documented by [`bleu/byos-contracts` ADR-0005](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0005-trampoline-execution-authority.md) and are only referenced here, not restated. This ADR keeps the service-owned decisions: the HTTP API surface, validation pipeline, rate limiting, process topology, and persistence.
>
> Revised 2026-07 during the COW-1159 review (COW-1173): ingestion validation switched from synchronous to asynchronous. The request path now does signature checking only; escrow and simulation run in a background validator. The original synchronous design is preserved under Alternatives.

## Context

The public HTTP API by which sub-solvers submit signed proposals. Endpoints (RFP):
- `POST /proposals` — `{order_uid, sell_amount, buy_amount, interactions, valid_until, nonce, signature}`; token addresses come from the orderbook order, not the sub-solver ([ADR-0012](0012-simulation.md))
- `GET /proposals/{order_uid}` — metadata only, never full contents (no leakage channel)
- `DELETE /proposals/{id}` — cancellation by the original signer

## Decision

The API surface, authentication model, ingestion pipeline, rate limiting structure, two-listener topology, and persistence model are specified in the design document (see Spec link above). This ADR records *why* each choice was made.

### Authentication: EIP-712 signature, signer is the identity

The recovered signer address doubles as escrow key and Trampoline CREATE2 salt — no separate `escrow_account` field and no delegation in v1. Signing structs and domain parameters are consumed from the contracts repo, never redefined here. No off-chain nonce bookkeeping: `GPv2Settlement`'s fill tracking prevents replay; `valid_until` bounds the window.

### Proposal payload shape: raw interactions

Restricting to BYOS-known venues (structured routes) would defeat the permissionless any-DEX value proposition. Containment of arbitrary calls is the Trampoline's job, structurally ([contracts ADR-0001](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0001-trampoline-topology.md)); the service's role is accept-or-reject at gatekeeping, never patching.

### Cancellation: EIP-712 signed, by server-assigned ID

`CancelProposal` is purely an API-authentication type — it is never verified on-chain, so it is owned by this repo. Follows the CoW pattern (CoW uses `OrderCancellation { orderUid }` for order cancellations).

### GET metadata

Owned by [ADR-0011](0011-owner-scoped-reads.md): reads are signature-gated and owner-scoped.

### Proposal lifecycle

Owned by [ADR-0013](0013-proposal-lifecycle-and-retention.md): the state machine, transition rules, rejection semantics, and retention.

What this ADR keeps: proposals are immutable — amounts, interactions, `validUntil`, nonce, and signature form one signed unit, so there is no update operation. Replacement is a new `POST`, which is why the API has no `PUT`.

### Ingestion validation: async, signature-only request path

The async-ingestion rule bans RPC and simulation from the request path; cheap in-memory checks and the local store write are fine. The reject-early pipeline stages are specified in the design document. The choice to go async was made during the COW-1159 review: synchronous ingestion puts RPC calls on the public request path, ties latency to RPC health, and forces two code paths doing the same judgment.

### Rate limiting: two-layer, escrow-tiered

Both numbers are placeholders — this ADR commits to the two-layer structure, and the actual limits are operational tuning parameters set at deployment. The authoritative escrow check that gates settlement happens at `/solve` selection ([ADR-0002](0002-solver-engine.md)).

### API topology: two listeners, one process

Separate listeners prevent public traffic from starving `/solve` of resources. Network-level isolation is straightforward (firewall the internal port).

### Persistence: Postgres store + async write-behind audit trail

The proposal store is owned by [ADR-0013](0013-proposal-lifecycle-and-retention.md). What stays here is the audit trail rationale:

- **Events, not snapshots.** Disputes care about what happened when; the writer never does read-modify-write.
- **Emission is in the store, by construction.** Every mutation emits an event into an unbounded channel. New mutation paths cannot forget to leave evidence.
- **No deletion path.** The 3-month window is a floor, not a TTL ([ADR-0013](0013-proposal-lifecycle-and-retention.md) reconfirmed keeping the log indefinitely for now).

## Alternatives considered

Contract-side alternatives (BYOS-unilateral execution, amounts-only signing without `interactionsHash`, delegated collateral via an `escrow_account` field, on-chain nonce enforcement) are recorded in [contracts ADR-0005](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0005-trampoline-execution-authority.md). Service-side:

- **Structured routes instead of raw interactions.** BYOS encodes every low-level call, can forbid sub-solver approvals entirely. Rejected — kills any-DEX generality, requires BYOS to maintain a venue registry, bottlenecks sub-solver innovation.
- **GET returns count only (Level 1).** Minimal leakage. Rejected — sub-solvers need per-proposal metadata to manage submissions and discover IDs for cancellation.
- **GET returns amounts (Level 2 with pricing).** Rejected — amounts reveal pricing strategy, the most competitively sensitive data.
- **Temporary suspension on simulation failure (retry loop).** Keeps failed proposals and re-simulates periodically. Rejected — adds complexity, wastes simulation cycles, and sub-solvers naturally resubmit via their polling loops.
- **Escrow slash on simulation failure.** Debit sub-solvers whose proposals fail simulation, both as a spam deterrent and as a buffer for penalties BYOS cannot pass through. Rejected — simulation failures are usually environmental (pool state moved, order filled elsewhere), not misbehavior, so slashing them would punish honest participants and deter permissionless participation. Debits are reserved for provable faults ([ADR-0003](0003-slash-attribution-flow.md)); unattributable penalty shortfalls are absorbed by BYOS by design, and spam is handled by rate limiting.
- **Synchronous ingestion (inline pipeline, verdict in the response).** The original v1 choice: run escrow check + simulation inline and answer `POST` with the final verdict — immediate feedback, no polling for rejection reasons, the only heavy step being a single simulation `eth_call` (tens of milliseconds). Reversed during the COW-1159 review: it puts RPC calls and simulation on the public request path, ties request latency and error behavior to RPC health, and forces the inline pipeline and the re-simulation loop to exist as two code paths doing the same judgment. The feedback-latency objection that originally killed async is softened by sub-solvers' existing polling loops and a tick interval targeting one block, not 3–5.
- **Eager per-submission validation task (async, but validate immediately).** Keep the request path signature-only, but fire a background task per submission instead of waiting for the next loop tick — near-immediate verdicts. Rejected — two validation entry points to keep consistent for a latency win the polling loop doesn't need; the loop interval already bounds verdict delay to ~one block.
- **Single listener for both public API and /solve.** Simpler, but public traffic can starve the latency-critical `/solve` endpoint. Rejected — `/solve` latency is a hard SLA.
- **Pure in-memory (no persistence).** Simplest, but loses dispute evidence on restart. Track B claims arrive months later. Rejected — the audit trail is required by the slashing policy.
- **In-memory hot store (memory-only /solve, resubmission on restart).** The original choice here, to keep DB queries off the auction-critical path. Reversed by [ADR-0013](0013-proposal-lifecycle-and-retention.md): at actual volumes an indexed Postgres read is ~1 ms against a seconds-scale deadline, and durability removes the restart-resubmission failure mode.

## Consequences

- **`POST` no longer answers with a verdict.** The `id` in the `202` means "accepted for validation"; sub-solver clients must poll `GET /proposal/{id}` to learn `active`/`rejected` and read the typed `rejectionReason`. Integration code written against the synchronous contract (treating a `2xx` as acceptance) is wrong under this design.
- **Verdict latency is bounded by the validator tick interval** (default 12s), not by the request round-trip. Simulation (COW-1162) must be built inside the background validator from the start — not inline and then moved.
- **Sub-solvers must include all required interactions (hooks, approvals) in their proposals.** BYOS can reject at gatekeeping but cannot patch proposals post-submission. A sub-solver who omits required hooks will be rejected; one who passes gatekeeping but causes an EBBO violation is still liable (gatekeeping is non-exculpatory per [ADR-0003](0003-slash-attribution-flow.md)).
- **The signing schema is an external dependency.** The `ProposalData` struct, typehash, and domain are fixed by the contracts repo; a contracts redeployment (v2 factory) invalidates all outstanding signatures, and sub-solver clients (including `subsolver` and `proposal-dto` here) must update their domain configuration. Signature code in this repo must be tested against contract-provided vectors, not a local re-derivation.
- **Audit trail becomes an operational dependency for dispute resolution.** If the audit log is lost or corrupted, BYOS cannot prove attribution for Track B claims and must absorb the cost. Requires backup/retention policy.
- **Rate limiting by escrow balance creates a pay-to-play throughput gradient.** Well-capitalized sub-solvers get higher rate limits. Accepted — consistent with the collateral-gated permission model, and prevents under-collateralized signers from consuming simulation resources.
