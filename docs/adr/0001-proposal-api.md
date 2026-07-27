# Proposal API & sub-solver authorization

Status: accepted

> Ported from [`bleu/cow-byos-architecture` ADR-0004](https://github.com/bleu/cow-byos-architecture/blob/main/docs/adr/0004-proposal-api.md), where it was accepted during the grant proposal. The original ADR also settled the contract-side halves of this decision — the signature-gated `execute`, the EIP-712 `ProposalData` schema, and the factory-anchored domain. Those are owned and documented by [`bleu/byos-contracts` ADR-0005](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0005-trampoline-execution-authority.md) and are only referenced here, not restated. This ADR keeps the service-owned decisions: the HTTP API surface, validation pipeline, rate limiting, process topology, and persistence.
>
> Revised 2026-07 during the COW-1159 review (COW-1173): ingestion validation switched from synchronous to asynchronous. The request path now does signature checking only; escrow and simulation run in a background validator. The original synchronous design is preserved under Alternatives.

## Context

The public HTTP API by which sub-solvers submit signed proposals. Endpoints (RFP):
- `POST /proposals` — `{order_uid, sell_amount, buy_amount, interactions, valid_until, nonce, signature}`; token addresses come from the orderbook order, not the sub-solver ([ADR-0012](0012-simulation.md))
- `GET /proposals/{order_uid}` — metadata only, never full contents (no leakage channel)
- `DELETE /proposals/{id}` — cancellation by the original signer

## Decision

### Authentication: EIP-712 signature, signer is the identity

Every proposal carries an EIP-712 signature over the `ProposalData` struct defined in [contracts ADR-0005](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0005-trampoline-execution-authority.md), which owns the typehash, the `interactionsHash` commitment, and the domain (anchored to the TrampolineFactory as `verifyingContract`). The same signature the service verifies at ingestion is later verified on-chain by the Trampoline at settlement, so what the service accepts is exactly what the sub-solver consented to execute.

The service-side implications this ADR commits to:

- **The recovered signer address IS the sub-solver's identity**: it is the escrow key for collateral checks and the CREATE2 salt for its Trampoline instance. There is no separate `escrow_account` field and no delegation in v1 — a sub-solver who wants multiple strategies deposits separately per address.
- **Signing structs and domain parameters are consumed from the contracts repo, never redefined here.** The `subsolver` and `proposal-dto` crates must produce hashes that verify against the deployed contracts; contract test vectors are the source of truth.
- **No off-chain nonce bookkeeping.** The nonce is a unique salt for signature uniqueness; the service enforces no ordering or uniqueness (mirroring the storage-free contract design). Replay of a settled proposal is prevented by `GPv2Settlement`'s fill tracking; `valid_until` bounds the window.

### Proposal payload shape: raw interactions

`Vec<{target, value, calldata}>` — the sub-solver encodes arbitrary calls against any DEX or protocol; the service passes them through for execution as-is inside the sub-solver's Trampoline.

Restricting to BYOS-known venues (structured routes) would defeat the permissionless any-DEX value proposition. Containment of arbitrary calls is the Trampoline's job, structurally ([contracts ADR-0001](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0001-trampoline-topology.md)); the service's role is accept-or-reject at gatekeeping, never patching. The sub-solver is fully responsible for the complete route, including required hooks and approvals.

### Cancellation: EIP-712 signed, by server-assigned ID

`DELETE /proposals/{id}` requires an EIP-712 signed cancellation message:

```solidity
struct CancelProposal {
    uint256 proposalId;
}
```

Same domain as proposals. `CancelProposal` is purely an API-authentication type — it is never verified on-chain, so it is owned by this repo. BYOS recovers the signer, verifies it matches the proposal's solver, then deletes. Follows the CoW pattern (CoW uses `OrderCancellation { orderUid }` for order cancellations). Sub-solvers discover their proposal IDs via `GET /proposals/{order_uid}`.

### GET metadata

Owned by [ADR-0011](0011-owner-scoped-reads.md): reads are signature-gated and owner-scoped.

### Proposal lifecycle

Owned by [ADR-0013](0013-proposal-lifecycle-and-retention.md): the state machine, transition rules (compare-and-swap status writes), rejection semantics, and retention.

What this ADR keeps: proposals are immutable. Amounts, interactions, `validUntil`, nonce, and signature form one signed unit, so there is no update operation on an existing proposal. Replacement is a new `POST` (optionally preceded by a `DELETE` of the old one) — which is why the API has no `PUT`.

### Ingestion validation: async, signature-only request path

`POST /proposals` does three things inline: parse the request, recover the signer (`ecrecover`), and check the expiry window (`valid_until` must be in the future but no further out than the lifetime cap, [ADR-0013](0013-proposal-lifecycle-and-retention.md)). On success it stores the proposal as `Submitted` and answers `202 Accepted` with the proposal `id` — meaning "accepted for validation," not "accepted." Signature and expiry-window failures reject synchronously with a 4xx — there is no point accepting, storing, and auditing a proposal that is dead on arrival.

All on-chain work — the escrow balance check and the simulation `eth_estimateGas` ([ADR-0012](0012-simulation.md)) — runs in a background validator loop, off the request path. Each tick (configurable interval, default 12s) sweeps expired proposals, then validates every `Submitted` and `Active` proposal. `Submitted` proposals are flipped to `Active`, `Rejected`, or `SimFailed`; `Active` proposals are re-validated and flipped to `SimFailed` if the simulation now reverts. Sub-solvers poll `GET /proposal/{id}` for the verdict; a rejection carries its typed reason.

### Rate limiting: two-layer, escrow-tiered

1. **IP-based coarse filter** — a generous per-IP limit (e.g., 100 req/s) plus a service-wide ceiling, for DDoS protection, applied before any cryptography. The service-wide cap bounds multi-IP floods that stay under the per-IP limit.
2. **Signer-based fine limit** — applied after `ecrecover`. Base rate (e.g., 10 proposals/min per signer), scaled by escrow balance tier. Sub-solvers below minimum escrow are rejected entirely.

The two layers are independent: the IP filter sheds floods before any cryptography; the signer limit caps each identity after recovery. Both numbers are placeholders — this ADR commits to the two-layer structure, and the actual limits are operational tuning parameters set at deployment.

Escrow balance is cached with a short TTL (~1 block period) for rate-limiting. The per-request check is an in-memory read against that cache — no RPC on the request path; refreshing costs one call per known sub-solver per block. The authoritative escrow check that gates actual settlement happens at `/solve` selection ([ADR-0002](0002-solver-engine.md)). The reject-early pipeline, split across the sync/async boundary — the async-ingestion rule bans RPC and simulation from the request path; cheap in-memory checks and the local store write are fine:

Synchronous (request path):
1. IP filter (shed floods)
2. Parse + `ecrecover` (identify signer)
3. Expiry-window check (`valid_until` in the past, or beyond the lifetime cap of [ADR-0013](0013-proposal-lifecycle-and-retention.md) → 4xx before storing)
4. Signer rate limit check (shed per-identity spam)
5. Cached escrow balance tier check (shed ineligible signers, in-memory read)

Background validator:
6. Authoritative escrow balance check (RPC)
7. Gatekeeping + simulation `eth_estimateGas` ([ADR-0012](0012-simulation.md); expensive, only for eligible proposals)

### API topology: two listeners, one process

- **Public port** — `/proposals` endpoints (POST, GET, DELETE). Public internet, rate-limited, authenticated.
- **Internal port** — `/solve` endpoint. Called only by CoW driver/autopilot, trusted, latency-critical.

Separate listeners prevent public traffic from starving `/solve` of resources. Both run in one process against the same proposal store. Network-level isolation is straightforward (firewall the internal port).

### Persistence: Postgres store + async write-behind audit trail

The proposal store — current state, a Postgres `proposals` table, single source of truth — is owned by [ADR-0013](0013-proposal-lifecycle-and-retention.md), along with its retention policy. What stays here is the audit trail:

- **Audit trail** — proposal lifecycle events are asynchronously persisted to Postgres (via sqlx) as an append-only `audit_events` log, for dispute evidence. [ADR-0003](0003-slash-attribution-flow.md) requires BYOS to map settlements back to proposals for Track A debits and Track B passthrough. Track B claims arrive up to 3 months later — the audit log must retain proposals for at least that window.

The `proposals` table holds what *is*; `audit_events` holds what *happened*. Separating the two avoids conflating their lifecycle requirements. Write-behind mechanics (COW-1172):

- **Events, not snapshots.** One row per lifecycle event (`received` with the full signed proposal body, `cancelled`, status changes, and driver-reported outcomes per [ADR-0010](0010-settlement-outcome-source.md)). Disputes care about what happened when; the writer never does read-modify-write.
- **Emission is in the store, by construction.** Every mutation of the proposal store emits an event into an unbounded channel; a writer task drains it into Postgres. New mutation paths cannot forget to leave evidence.
- **Fail-fast boot, retry-forever runtime, drain on shutdown.** The service refuses to start without a reachable database and applied migrations; a runtime outage queues events in memory while the writer retries with backoff; graceful shutdown flushes the queue before exit.
- **No deletion path.** The 3-month window is a floor, not a TTL. Any future retention policy must also cover dispute-processing time beyond claim arrival ([ADR-0013](0013-proposal-lifecycle-and-retention.md) reconfirmed keeping the log indefinitely for now).

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
