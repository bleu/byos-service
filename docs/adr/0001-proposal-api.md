# Proposal API & sub-solver authorization

Status: accepted

> Ported from [`bleu/cow-byos-architecture` ADR-0004](https://github.com/bleu/cow-byos-architecture/blob/main/docs/adr/0004-proposal-api.md), where it was accepted during the grant proposal. The original ADR also settled the contract-side halves of this decision — the signature-gated `execute`, the EIP-712 `ProposalData` schema, and the factory-anchored domain. Those are owned and documented by [`bleu/byos-contracts` ADR-0005](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0005-trampoline-execution-authority.md) and are only referenced here, not restated. This ADR keeps the service-owned decisions: the HTTP API surface, validation pipeline, rate limiting, process topology, and persistence.

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

### Proposal lifecycle: permanent drop on simulation failure

Five lifecycle events terminate a proposal:
1. **`valid_until` expires** — BYOS drops it.
2. **Settled** — order filled, proposal consumed.
3. **Order canceled/filled by another solver** — order UID no longer valid.
4. **Simulation failure** — proposal reverts against current block → permanently dropped. No suspension/retry. Sub-solver resubmits if conditions change.
5. **Explicit cancellation** — signed `DELETE`.

Permanent drop on simulation failure keeps the store simple (proposals are either active or gone), reduces simulation load (no re-checking failed proposals), and ensures fresh gatekeeping on resubmission. Sub-solvers run continuous polling loops and naturally resubmit.

Proposals are immutable. Amounts, interactions, `validUntil`, nonce, and signature form one signed unit, so there is no update operation on an existing proposal. Replacement is a new `POST` (optionally preceded by a `DELETE` of the old one) — which is why the API has no `PUT`.

### Ingestion validation: synchronous, with verdict

`POST /proposals` runs the full validation pipeline inline and answers with the verdict: the proposal `id` on acceptance, a 4xx with a machine-readable reason on rejection. Sub-solvers need immediate feedback to iterate; silent drops or deferred verdicts make integration painful. The only heavy step is a single simulation `eth_call` (tens of milliseconds), and rate limiting bounds the aggregate load.

### Rate limiting: two-layer, escrow-tiered

1. **IP-based coarse filter** — a generous per-IP limit (e.g., 100 req/s) plus a service-wide ceiling, for DDoS protection, applied before any cryptography. The service-wide cap bounds multi-IP floods that stay under the per-IP limit.
2. **Signer-based fine limit** — applied after `ecrecover`. Base rate (e.g., 10 proposals/min per signer), scaled by escrow balance tier. Sub-solvers below minimum escrow are rejected entirely.

The two layers are independent: the IP filter sheds floods before any cryptography; the signer limit caps each identity after recovery. Both numbers are placeholders — this ADR commits to the two-layer structure, and the actual limits are operational tuning parameters set at deployment.

Escrow balance is cached with a short TTL (~1 block period) for rate-limiting. The per-request check is an in-memory read against that cache — no RPC on the request path; refreshing costs one call per known sub-solver per block. The authoritative escrow check that gates actual settlement happens at `/solve` selection ([ADR-0002](0002-solver-engine.md)). The reject-early pipeline:
1. IP filter (shed floods)
2. Parse + `ecrecover` (identify signer)
3. Signer rate limit check (shed per-identity spam)
4. Cached escrow balance check (shed ineligible signers)
5. Gatekeeping + simulation (expensive, only for eligible proposals)

### API topology: two listeners, one process

- **Public port** — `/proposals` endpoints (POST, GET, DELETE). Public internet, rate-limited, authenticated.
- **Internal port** — `/solve` endpoint. Called only by CoW driver/autopilot, trusted, latency-critical.

Separate listeners prevent public traffic from starving `/solve` of resources. The proposal store is shared in-memory within the single process. Network-level isolation is straightforward (firewall the internal port).

### Persistence: in-memory hot path + async write-behind

- **Hot store** — in-memory (`RwLock<HashMap>` or equivalent). Serves `/solve` with no DB query on the auction-critical path. Rebuilt from fresh submissions on restart.
- **Audit trail** — proposals are asynchronously persisted (SQLite WAL, flat log, or equivalent) for dispute evidence. [ADR-0003](0003-slash-attribution-flow.md) requires BYOS to map settlements back to proposals for Track A debits and Track B passthrough. Track B claims arrive up to 3 months later — the audit log must retain proposals for at least that window.

Hot proposals live minutes; audit records live months. Separating the two avoids conflating their lifecycle requirements.

## Alternatives considered

Contract-side alternatives (BYOS-unilateral execution, amounts-only signing without `interactionsHash`, delegated collateral via an `escrow_account` field, on-chain nonce enforcement) are recorded in [contracts ADR-0005](https://github.com/bleu/byos-contracts/blob/main/docs/adr/0005-trampoline-execution-authority.md). Service-side:

- **Structured routes instead of raw interactions.** BYOS encodes every low-level call, can forbid sub-solver approvals entirely. Rejected — kills any-DEX generality, requires BYOS to maintain a venue registry, bottlenecks sub-solver innovation.
- **GET returns count only (Level 1).** Minimal leakage. Rejected — sub-solvers need per-proposal metadata to manage submissions and discover IDs for cancellation.
- **GET returns amounts (Level 2 with pricing).** Rejected — amounts reveal pricing strategy, the most competitively sensitive data.
- **Temporary suspension on simulation failure (retry loop).** Keeps failed proposals and re-simulates periodically. Rejected — adds complexity, wastes simulation cycles, and sub-solvers naturally resubmit via their polling loops.
- **Escrow slash on simulation failure.** Debit sub-solvers whose proposals fail simulation, both as a spam deterrent and as a buffer for penalties BYOS cannot pass through. Rejected — simulation failures are usually environmental (pool state moved, order filled elsewhere), not misbehavior, so slashing them would punish honest participants and deter permissionless participation. Debits are reserved for provable faults ([ADR-0003](0003-slash-attribution-flow.md)); unattributable penalty shortfalls are absorbed by BYOS by design, and spam is handled by rate limiting.
- **Async ingestion (202 + status polling).** Accept immediately with status `pending`, validate in the continuous-simulation loop ([ADR-0002](0002-solver-engine.md)), flip status to `active`/`rejected`. Rejected for v1 — the loop's 3–5 block interval delays feedback by up to a minute and forces sub-solvers to poll just to learn a rejection reason. Kept as the scaling fallback if synchronous simulation latency ever becomes a bottleneck.
- **Single listener for both public API and /solve.** Simpler, but public traffic can starve the latency-critical `/solve` endpoint. Rejected — `/solve` latency is a hard SLA.
- **Pure in-memory (no persistence).** Simplest, but loses dispute evidence on restart. Track B claims arrive months later. Rejected — the audit trail is required by the slashing policy.
- **Fully durable hot store (DB-backed /solve).** Proposals survive restarts without resubmission, but adds DB latency to the auction-critical path. Rejected — sub-solver resubmission on restart is acceptable given short proposal lifetimes and continuous polling.

## Consequences

- **Sub-solvers must include all required interactions (hooks, approvals) in their proposals.** BYOS can reject at gatekeeping but cannot patch proposals post-submission. A sub-solver who omits required hooks will be rejected; one who passes gatekeeping but causes an EBBO violation is still liable (gatekeeping is non-exculpatory per [ADR-0003](0003-slash-attribution-flow.md)).
- **The signing schema is an external dependency.** The `ProposalData` struct, typehash, and domain are fixed by the contracts repo; a contracts redeployment (v2 factory) invalidates all outstanding signatures, and sub-solver clients (including `subsolver` and `proposal-dto` here) must update their domain configuration. Signature code in this repo must be tested against contract-provided vectors, not a local re-derivation.
- **Pre-settlement information leakage via GET.** Solver addresses per order are visible before settlement. An observer can map which sub-solvers are competing on which orders. Accepted — addresses are recoverable post-settlement from on-chain data anyway, and the v1 sub-solver set is expected to be small.
- **BYOS restart requires sub-solver resubmission.** The in-memory hot store is lost. Mitigated by short proposal lifetimes and sub-solvers' continuous polling loops — proposals are naturally refreshed within one poll interval.
- **Audit trail becomes an operational dependency for dispute resolution.** If the audit log is lost or corrupted, BYOS cannot prove attribution for Track B claims and must absorb the cost. Requires backup/retention policy.
- **Rate limiting by escrow balance creates a pay-to-play throughput gradient.** Well-capitalized sub-solvers get higher rate limits. Accepted — consistent with the collateral-gated permission model, and prevents under-collateralized signers from consuming simulation resources.
