# Settlement outcome source & Track A trigger

Status: accepted

> Resolves the "driver callbacks for outcome observation" open question from [ADR-0002](0002-solver-engine.md). Uses the slashing policy in [ADR-0003](0003-slash-attribution-flow.md). Rewritten 2026-07 during the proposal-lifecycle review ([ADR-0013](0013-proposal-lifecycle-and-retention.md)): the original version proposed forking the driver to add an outcome hook; the stock driver turned out to already send everything we need.

## Context

When a BYOS settlement fails, we charge the responsible sub-solver (Track A, [ADR-0003](0003-slash-attribution-flow.md)). To do that we need three things: to know it failed, the gas it cost, and which sub-solver to charge.

The first plan was a chain watcher: scan every block, find our settlement, check if it reverted. The second plan (this ADR's original version) was a driver fork with a custom outcome hook, since the driver — the process that submits the settlement and watches how it lands — already knows the outcome, and CoW runs the driver for us under the bonding pool arrangement.

Then we checked what the standard driver↔solver-engine protocol already carries. In [`cowprotocol/services`](https://github.com/cowprotocol/services) (`crates/solvers-dto/src/notification.rs`), the driver notifies its solver engine per solution via `POST /notify`, with auction and solution ids and a kind that covers the full submission lifecycle:

- `SettlementStarted` — our solution won and the driver began submitting the tx.
- `Success { transaction }` — settled, with tx hash.
- `Revert { transaction }` — reverted on-chain, with tx hash.
- `Cancelled` / `Expired` / `Fail` — the submission was abandoned or missed the deadline; no tx landed.
- Pre-submission rejections (`SimulationFailed`, `EmptySolution`, `DuplicatedSolutionId`, ...).

The driver handles private submissions and dropped txs, so these notifications cover cases a block scanner would miss. What the driver does not carry is the mined gas — it knows the tx hash and whether it reverted, not what it cost.

## Decision

Get the outcome from the stock driver's `/notify` notifications. No fork, no chain watcher.

- **Trigger.** BYOS implements the solver-engine `/notify` endpoint (internal listener). `Revert { transaction }` is the Track A trigger; `Success` closes the proposal as settled. The full notification→state mapping is owned by [ADR-0013](0013-proposal-lifecycle-and-retention.md).
- **Attribution.** Notifications carry auction and solution ids, not proposals. `/solve` records `(auction_id, solution_id, proposal_id)` in the `solutions` table before returning a solution (ADR-0013), so attribution is a join in our own database. The Trampoline address in the settlement calldata remains the on-chain proof, checked when we debit.
- **Gas.** BYOS makes one `eth_getTransactionReceipt` call on the reverted tx hash to read the real gas used and gas price. No block scanning, no subscriptions. The receipt read is required, not a double-check: it is how we get the amount to debit.
- **Escrow stays native.** It applies `gas + c_l` for Track A and handles Track B. Track B (freeze/debit on a CoW ruling) is triggered by hand, not by a chain event.

## Alternatives considered

- **Driver fork with a custom outcome hook** (this ADR's original decision). Obsolete: the stock `/notify` protocol already delivers the outcome per solution, including tx hashes. The bleu driver fork (COW-1190) still exists for other needs — fee configuration — but outcome observation is no longer among its reasons, and this ADR no longer depends on it.
- **Chain watcher (scan blocks).** Rejected. Rebuilds what the driver does, misses private and dropped txs, and cannot spot a missed deadline without knowing what we sent. Fallback only if notifications prove unreliable in practice.
- **Autopilot observations.** Rejected. CoW-run, tied to its database, wrong attribution level (solver, not sub-solver), gas table dropped (migration `V090`). Useful as a reference for the receipt read, not as a source.
- **Shepherd (WASM) module.** Rejected here. Shepherd earns its place when you subscribe to chain events; we have a push plus one read. Still a fit for event-driven jobs like TWAP or EthFlow.
- **Driver applies the penalty.** Rejected. Keys and the debit decision stay in our audited escrow code, and the driver cannot do Track B.

## Consequences

- No chain indexer to build or run, and outcome observation has no dependency on CoW accepting a fork — the notification protocol is how every driver talks to its solver engine. (The fork tracked in COW-1190 remains, for fee configuration — a separate concern.)
- BYOS must expose `/notify` and keep the `solutions` mapping durable ([ADR-0013](0013-proposal-lifecycle-and-retention.md)); a notification that cannot be attributed is an alert-worthy bug.
- Missed-deadline detection is free, from the driver's `Expired`/`Cancelled` notifications.
- Private submissions are covered, because the driver is the source.
- Lost notifications are survivable for liveness: ADR-0013's executing-timeout returns the proposal to `Active`, and re-simulation reconciles reality. A lost `Revert` does cost the Track A debit for that settlement unless recovered by hand from the audit trail and chain — acceptable at expected volumes, revisit if it ever happens.
- The "won the auction but chose not to settle" Track A case is out of scope here. It has no tx and comes from our own auction records (the `solutions` table now provides them).
- We are not using Shepherd for this. Written down so we do not revisit it.
