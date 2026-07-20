# Proposal API & sub-solver authorization

Status: accepted

> Ported from [`bleu/cow-byos-architecture` ADR-0004](https://github.com/bleu/cow-byos-architecture/blob/main/docs/adr/0004-proposal-api.md), where it was accepted during the grant proposal. The original ADR also settled the contract-side halves of this decision — the signature-gated `execute`, the EIP-712 `ProposalData` schema, and the factory-anchored domain. Those are owned and documented by [`bleu/byos-contracts` ADR-0005](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0005-trampoline-execution-authority.md) and are only referenced here, not restated. This ADR keeps the service-owned decisions: the HTTP API surface, validation pipeline, rate limiting, process topology, and persistence.
>
> Revised 2026-07 during the COW-1159 review (COW-1173): ingestion validation switched from synchronous to asynchronous. The request path now does signature checking only; escrow and simulation run in a background validator. The original synchronous design is preserved under Alternatives.

## Context

The public HTTP API by which sub-solvers submit signed proposals. Endpoints (RFP):
- `POST /proposals` — `{order_uid, sell_amount, buy_amount, interactions, valid_until, nonce, signature}`
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

### GET metadata: per-proposal, no amounts

> **Superseded by [ADR-0011](0011-owner-scoped-reads.md):** reads are now signature-gated and owner-scoped; the public-metadata trade-off below no longer holds.

`GET /proposals/{order_uid}` returns:

```json
{
  "proposals": [
    { "id": 42, "solver": "0xabc...", "validUntil": 1750000000, "status": "active" },
    { "id": 43, "solver": "0xdef...", "validUntil": 1750000060, "status": "active" }
  ]
}
```

Per-proposal metadata: `id`, `solver`, `validUntil`, `status`. **No amounts, no interaction data.** Amounts would reveal pricing strategy; interactions would reveal routing — both are competitively sensitive. Solver addresses are already recoverable from on-chain settlement calldata (Trampoline CREATE2 address maps to sub-solver), so pre-settlement address visibility is an accepted trade-off.

The public per-order view lists `Active` proposals only — a `Submitted` proposal has not passed gatekeeping, and showing it would leak submission attempts BYOS has not vouched for. The owner's own management view (`GET /proposals/by-solver/{address}`) includes `Submitted` alongside `Active`, so a sub-solver can see pending work.

### Proposal lifecycle: explicit state machine, permanent drop on failure

A proposal enters as `Submitted` (signature verified, awaiting validation) and moves through enforced transitions — every status write is a compare-and-swap against the expected current state, so concurrent events (a cancellation racing the validator) cannot resurrect or overwrite each other; the stale write is dropped:

- `Submitted → Active` — background validation passed.
- `Submitted → Rejected` — failed a gatekeeping rule (e.g. insufficient escrow, order no longer in the orderbook). Carries a machine-readable `rejectionReason` (PascalCase enum per [ADR-0007](0007-error-handling.md)), exposed on `GET /proposal/{id}`.
- `Submitted | Active → SimFailed` — simulation reverted.
- `Submitted | Active → Expired` — `valid_until` passed.
- `Submitted | Active → Cancelled` — signed `DELETE`. Only live proposals can be cancelled; a `DELETE` against a terminal state is a `409`.
- `Active → Settled` — order filled by this proposal.
- `Rejected`, `SimFailed`, `Expired`, `Cancelled`, `Settled` are terminal.

`Rejected` and `SimFailed` are deliberately distinct verdicts: `Rejected` means "you are not eligible" (fix: deposit escrow), `SimFailed` means "your route does not work against current chain state" (fix: rebuild the route). Both are permanent — no suspension/retry. Permanent drop keeps the store simple, reduces simulation load (no re-checking failed proposals), and ensures fresh gatekeeping on resubmission; sub-solvers run continuous polling loops and naturally resubmit.

Order-death signals (order filled by another solver, or cancelled in the orderbook) come from the driver, not from a chain watcher ([ADR-0010](0010-settlement-outcome-source.md)) and not from the simulation (its `settle()` uses empty trades, so GPv2 fill-tracking is never exercised): the auction contents on `/solve` say which orders are still live (a heuristic — absence from one auction is not proof of death), and the driver's settlement outcome is authoritative for our own fills. Wiring these signals into the validator loop is follow-up work.

Proposals are immutable. Amounts, interactions, `validUntil`, nonce, and signature form one signed unit, so there is no update operation on an existing proposal. Replacement is a new `POST` (optionally preceded by a `DELETE` of the old one) — which is why the API has no `PUT`.

### Ingestion validation: async, signature-only request path

`POST /proposals` does exactly two things inline: parse the request and recover the signer (`ecrecover`). On success it stores the proposal as `Submitted` and answers `202 Accepted` with the proposal `id` — meaning "accepted for validation," not "accepted." Signature failures still reject synchronously with a 4xx.

All on-chain work — the escrow balance check and the simulation `eth_call` — runs in a background validator loop, off the request path. Each tick (configurable interval, default 12s, one mainnet block; block-driven ticking is a decision for the simulation work, COW-1162) sweeps expired proposals, then judges every `Submitted` proposal and flips it to `Active`, `Rejected`, or `SimFailed`. Sub-solvers poll `GET /proposal/{id}` for the verdict; a rejection carries its typed reason.

### Rate limiting: two-layer, escrow-tiered

1. **IP-based coarse filter** — a generous per-IP limit (e.g., 100 req/s) plus a service-wide ceiling, for DDoS protection, applied before any cryptography. The service-wide cap bounds multi-IP floods that stay under the per-IP limit.
2. **Signer-based fine limit** — applied after `ecrecover`. Base rate (e.g., 10 proposals/min per signer), scaled by escrow balance tier. Sub-solvers below minimum escrow are rejected entirely.

The two layers are independent: the IP filter sheds floods before any cryptography; the signer limit caps each identity after recovery. Both numbers are placeholders — this ADR commits to the two-layer structure, and the actual limits are operational tuning parameters set at deployment.

Escrow balance is cached with a short TTL (~1 block period) for rate-limiting. The per-request check is an in-memory read against that cache — no RPC on the request path; refreshing costs one call per known sub-solver per block. The authoritative escrow check that gates actual settlement happens at `/solve` selection ([ADR-0002](0002-solver-engine.md)). The reject-early pipeline, split across the sync/async boundary — everything on the request path is memory-only; the async-ingestion rule bans RPC and simulation from the sync side, not cheap in-memory checks:

Synchronous (request path):
1. IP filter (shed floods)
2. Parse + `ecrecover` (identify signer)
3. Signer rate limit check (shed per-identity spam)
4. Cached escrow balance tier check (shed ineligible signers, in-memory read)

Background validator:
5. Authoritative escrow balance check (RPC)
6. Gatekeeping + simulation `eth_call` (expensive, only for eligible proposals)

### API topology: two listeners, one process

- **Public port** — `/proposals` endpoints (POST, GET, DELETE). Public internet, rate-limited, authenticated.
- **Internal port** — `/solve` endpoint. Called only by CoW driver/autopilot, trusted, latency-critical.

Separate listeners prevent public traffic from starving `/solve` of resources. The proposal store is shared in-memory within the single process. Network-level isolation is straightforward (firewall the internal port).

### Persistence: in-memory hot path + async write-behind

- **Hot store** — in-memory (`RwLock<HashMap>` or equivalent). Serves `/solve` with no DB query on the auction-critical path. Rebuilt from fresh submissions on restart.
- **Audit trail** — proposal lifecycle events are asynchronously persisted to Postgres (via sqlx) as an append-only `audit_events` log, for dispute evidence. [ADR-0003](0003-slash-attribution-flow.md) requires BYOS to map settlements back to proposals for Track A debits and Track B passthrough. Track B claims arrive up to 3 months later — the audit log must retain proposals for at least that window.

Hot proposals live minutes; audit records live months. Separating the two avoids conflating their lifecycle requirements.

The write-behind path (COW-1172) supersedes this ADR's original "SQLite WAL, flat log, or equivalent" suggestion with Postgres: a managed instance gives the evidence off-host durability (a lost audit log means absorbing Track B costs), and it aligns with the M2 plan to move the store to Postgres. Mechanics:

- **Events, not snapshots.** One row per lifecycle event (`received` with the full signed proposal body, `cancelled`, and later driver-reported outcomes per [ADR-0010](0010-settlement-outcome-source.md)). Disputes care about what happened when; the writer never does read-modify-write.
- **Emission is in the store, by construction.** Every mutation of the in-memory store emits an event into an unbounded channel; a writer task drains it into Postgres. New mutation paths cannot forget to leave evidence.
- **Fail-fast boot, retry-forever runtime, drain on shutdown.** The service refuses to start without a reachable database and applied migrations; a runtime outage queues events in memory while the writer retries with backoff; graceful shutdown flushes the queue before exit.
- **The audit trail is the proposal-ID authority.** The in-memory ID counter reseeds from `max(proposal_id)` at boot, so restarts never reissue an ID and evidence rows stay unambiguous.
- **No deletion path.** The 3-month window is a floor, not a TTL. Any future retention policy must also cover dispute-processing time beyond claim arrival, and is deliberately left to a separate decision.

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
- **Fully durable hot store (DB-backed /solve).** Proposals survive restarts without resubmission, but adds DB latency to the auction-critical path. Rejected — sub-solver resubmission on restart is acceptable given short proposal lifetimes and continuous polling.

## Consequences

- **`POST` no longer answers with a verdict.** The `id` in the `202` means "accepted for validation"; sub-solver clients must poll `GET /proposal/{id}` to learn `active`/`rejected` and read the typed `rejectionReason`. Integration code written against the synchronous contract (treating a `2xx` as acceptance) is wrong under this design.
- **Verdict latency is bounded by the validator tick interval** (default 12s), not by the request round-trip. Simulation (COW-1162) must be built inside the background validator from the start — not inline and then moved.
- **Sub-solvers must include all required interactions (hooks, approvals) in their proposals.** BYOS can reject at gatekeeping but cannot patch proposals post-submission. A sub-solver who omits required hooks will be rejected; one who passes gatekeeping but causes an EBBO violation is still liable (gatekeeping is non-exculpatory per [ADR-0003](0003-slash-attribution-flow.md)).
- **The signing schema is an external dependency.** The `ProposalData` struct, typehash, and domain are fixed by the contracts repo; a contracts redeployment (v2 factory) invalidates all outstanding signatures, and sub-solver clients (including `subsolver` and `proposal-dto` here) must update their domain configuration. Signature code in this repo must be tested against contract-provided vectors, not a local re-derivation.
- **Pre-settlement information leakage via GET.** Solver addresses per order are visible before settlement. An observer can map which sub-solvers are competing on which orders. Accepted — addresses are recoverable post-settlement from on-chain data anyway, and the v1 sub-solver set is expected to be small. *Superseded by [ADR-0011](0011-owner-scoped-reads.md): this visibility no longer exists.*
- **BYOS restart requires sub-solver resubmission.** The in-memory hot store is lost. Mitigated by short proposal lifetimes and sub-solvers' continuous polling loops — proposals are naturally refreshed within one poll interval.
- **Audit trail becomes an operational dependency for dispute resolution.** If the audit log is lost or corrupted, BYOS cannot prove attribution for Track B claims and must absorb the cost. Requires backup/retention policy.
- **Rate limiting by escrow balance creates a pay-to-play throughput gradient.** Well-capitalized sub-solvers get higher rate limits. Accepted — consistent with the collateral-gated permission model, and prevents under-collateralized signers from consuming simulation resources.
