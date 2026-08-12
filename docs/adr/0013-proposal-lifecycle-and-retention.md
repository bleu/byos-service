# Proposal lifecycle & retention

Status: accepted

Spec: docs/shared/design-document.md#proposal-lifecycle
      https://bleu.github.io/byos-docs/design-document#proposal-lifecycle

## Context

The proposal state machine grew by accretion: `Submitted → Active` plus five terminal states, defined in `domain/proposal.rs` and described piecemeal across ADR-0001 (ingestion, persistence), ADR-0012 (simulation, re-validation), and ADR-0010 (settlement outcomes). Several things were missing or wrong as of 2026-07:

- **Nothing ever set `Settled`.** The state existed in the enum and the audit mapping, but no code path reached it — ADR-0010's outcome source was still a proposed driver fork.
- **Nothing happened between winning and the outcome.** A proposal picked by `/solve` stayed `Active`: it kept being re-simulated (and could flip to `SimFailed` because our own pending settlement was about to consume its balances) and kept being offered to the next auction while a settlement built on it was in flight.
- **Terminal proposals never left memory** (COW-1177): pruned from the secondary indexes, retained in the main map forever.
- **`validUntil` was unbounded.** Ingestion rejected timestamps in the past but accepted any future value, so one proposal could pin an `eth_estimateGas` per tick for the life of the process. ADR-0001's restart story ("short proposal lifetimes") was an assumption, not an invariant.
- **Retention was deliberately deferred.** ADR-0001 committed to a 3-month floor for audit evidence and punted the rest to "a separate decision". This is that decision.
- **The ingestion-time profitability gate** (ADR-0002 Q7) was decided in the 2026-07 review — reject unprofitable proposals — but had no write-up and no defined place in the lifecycle.

One discovery reshaped the settlement half: the stock CoW driver already notifies its solver engine per solution (`/notify` with `SettlementStarted`, `Success{transaction}`, `Revert{transaction}`, `Cancelled`, `Expired`, `Fail`, and several pre-submission rejection kinds, each carrying auction and solution ids). No driver fork is needed to observe outcomes — ADR-0010 is rewritten accordingly.

## Decision

The state machine, transition table, state properties, settlement outcome mapping, and retention windows are specified in the design document (see Spec link above). This ADR records *why* each design choice was made.

### Auction participation: losses are events, states are for winners

Losing an auction is **not a state**: the proposal stays `Active` and keeps competing. Participation is recorded as data (the `solutions` table plus audit events). Winning is a state change, because it changes what the service does: stop offering the proposal and stop re-simulating it.

### `Executing`: entered and exited by driver notifications

`Executing` is entered on `SettlementStarted`, not at `/solve` time — at `/solve` time we don't yet know we won. The window between the autopilot picking our solution and `SettlementStarted` arriving is sub-second-to-seconds; inventing a "won but not yet submitting" state isn't worth it.

Two safety properties:

- **`Executing → Active` is always safe.** If the order was actually consumed, the next re-simulation reverts and the proposal dies as `SimFailed`. Re-simulation is the truth-teller.
- **Timeout backstop.** An `Executing` proposal older than `--executing-timeout` (default 5 minutes) falls back to `Active` — covering lost notifications and restarts mid-settlement.

### Simulation stops at the first revert

Unchanged from ADR-0012, now with the lifecycle rationale: a proposal that reverted once is not robust enough to offer to `/solve` — if it won and then reverted on-chain, the sub-solver takes a Track A penalty, which is strictly worse than resubmitting. Resubmission is the sub-solver's "I still believe in this route" signal.

### Profitability gate: first simulation, not re-validation

Settles ADR-0002 Q7 (decided in the 2026-07 review). The gate is **not re-applied on re-validation** because gas prices wobble; rejecting an `Active` proposal on a spike would churn proposals that are profitable again two blocks later. `/solve` re-scores with fresh gas at auction time, so an unprofitable-right-now proposal cannot leak into a solution.

### Proposal lifetime is capped at ingestion

`--max-proposal-lifetime` (default 5 minutes) bounds worst-case simulation cost per proposal and turns ADR-0001's "short proposal lifetimes" assumption into an enforced invariant.

### `Penalized`: the Track A debit closes the story

The proposal row itself answers "was this sub-solver charged, and where's the proof" via `penalty_tx_hash`. Track B stays out of the proposal machine: a CoW ruling months later is an account-level event, not a transition of one proposal.

### Storage: Postgres is the proposal store

The in-memory hot store is removed. Key implementation decisions:

- **Compare-and-swap via SQL.** `UPDATE proposals SET status = $to WHERE id = $id AND status = $from` — zero rows means the caller's verdict was stale, exactly the in-memory semantics, now durable.
- **No cache, no Redis.** An indexed read over a few hundred live rows is ~1 ms. Premature until `/solve` measurements say otherwise.
- **Restarts stop being lossy.** Live proposals survive; sub-solver resubmission on restart is no longer part of the design.
- **A `solutions` table maps notifications back to proposals**: written synchronously inside `/solve` before the solution is returned — if we can't record it, we don't bid it.
- **The write-behind keeps a small crash window** (one event per crash). Accepted: closing it would couple every store write to the audit codec.

### Retention: one sweep, one knob

- **Dropped states: deleted 1 hour after reaching the state** (`--dropped-retention`).
- **Money states: kept indefinitely.** No sweep code at all.
- **`audit_events`: still no deletion path.**

New configuration: `--max-proposal-lifetime` (default 5m), `--executing-timeout` (default 5m), `--dropped-retention` (default 1h).

## Alternatives considered

- **"Lost auction" as a proposal state.** Rejected — the proposal would bounce `Active → Lost → Active` every auction while remaining fully valid; losing changes nothing about what the service does with it. Events answer the queries.
- **Enter `Executing` at `/solve` time.** Rejected — we don't know we won; every non-winning auction round would need an unwind path.
- **N-strikes or never-terminal `SimFailed`.** Rejected — a once-reverted proposal offered to `/solve` risks an on-chain revert and a penalty for the sub-solver; never-terminal burns an RPC call per tick on dead orders for their whole lifetime. (ADR-0001 records the same rejection as "temporary suspension on simulation failure".)
- **Re-applying the profitability gate on re-validation.** Rejected — gas-price flapping churns proposals that `/solve`'s own auction-time filter already handles.
- **In-memory terminal retention with a TTL (memory-only serving).** Rejected — a restart erases what a sub-solver needs to see about why its proposal died; "in memory at any stage is fragile" was the deciding argument, and it generalized to the whole store.
- **Redis (or any second store) for live proposals.** Rejected as premature — one database is enough at this volume; revisit only with `/solve` latency data.
- **Penalty as audit events only, no `Penalized` state.** Rejected — the debit tx hash belongs on the proposal so "was this charged" is a row lookup, not event-log archaeology.
- **Per-tier deletion windows for the money states (e.g. 90 days aligned to Track B).** Rejected for now to keep the sweep light; indefinite retention of a small set of settled rows costs nothing.

## Consequences

- **BYOS gains a `/notify` endpoint** on the internal listener, and `/solve` gains a synchronous `solutions` insert. Both are prerequisites for `Executing` and everything after it.
- **ADR-0010 is rewritten**: the outcome source is the stock driver's notifications, not a driver fork. Any fork discussion is now about other features only.
- **Breaking API changes for sub-solvers**: `validUntil` more than a few minutes out is rejected at `POST`; dropped proposals 404 an hour after dying; the status vocabulary grows (`executing`, `settleFailed`, `penalized`) and `GET` exposes `penalty_tx_hash` and settlement tx hashes.
- **ADR-0001 is trimmed where this ADR takes over** (lifecycle section, persistence hot-store half, restart consequence — each replaced by a pointer here). The two-listener topology, rate limiting, and authentication are untouched.
- **ADR-0012's re-validation section is edited in place**: only `Submitted` and `Active` simulate; the first pass additionally applies the profitability gate.
- **COW-1177 (terminal-proposal pruning) is resolved** by the retention sweep; the in-memory leak ceases to exist along with the in-memory store.
- **`POST` gets one DB write on the request path** (and `DELETE`/`GET` their reads). The async-ingestion rule from ADR-0001 — no RPC, no simulation on the sync side — still holds; a local indexed insert is not in that class.
- **Simulation load is bounded**: worst case one `eth_estimateGas` per proposal per tick for at most `--max-proposal-lifetime`.
- New configuration: `--max-proposal-lifetime` (default 5m), `--executing-timeout` (default 5m), `--dropped-retention` (default 1h).
