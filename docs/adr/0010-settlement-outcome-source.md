# Settlement outcome source & Track A trigger

Status: proposed

> Resolves the "driver callbacks for outcome observation" open question from [ADR-0002](0002-solver-engine.md). Uses the slashing policy in [ADR-0003](0003-slash-attribution-flow.md).

## Context

When a BYOS settlement fails, we charge the responsible sub-solver (Track A, [ADR-0003](0003-slash-attribution-flow.md)). To do that we need three things: to know it failed, the gas it cost, and which sub-solver to charge.

The first plan was a chain watcher: scan every block, find our settlement, check if it reverted. But the driver — the process that submits the settlement and watches how it lands — already knows the outcome. There is nothing to scan for; the question is how BYOS gets that signal out of the driver.

One operational fact shapes the answer: we will run the solver on a CoW bonding pool, which means CoW runs the driver for us — we do not operate our own driver instance. CoW has signaled they are open to running a fork of the driver with features we need, but this is not fully confirmed yet.

We checked this in [`cowprotocol/services`](https://github.com/cowprotocol/services):

- The driver classifies the outcome itself: `Outcome::Failed { reason: "Revert" }` and `{ reason: "Expired" }` (missed deadline), with the tx hash.
- The driver handles private submissions and dropped txs. A block scanner would miss those.
- The driver does not read the mined gas, only whether the tx reverted.
- The autopilot does read gas from the receipt, but we can't use it: it is CoW-run, tied to its own database, attributes at the solver (not sub-solver) level, and its gas table was dropped.

## Decision

Get the outcome from the driver, not from a chain watcher.

- **Trigger.** Fork the driver and add a hook that sends BYOS the outcome (tx hash + Revert/Expired) when a submission finishes. Small change. CoW runs this fork under the bonding pool arrangement — they seem open to it, pending final confirmation.
- **Attribution.** We already know the sub-solver — we picked its proposal and built the settlement. It is a lookup in our own records. The Trampoline address in the calldata is the on-chain proof, checked when we debit.
- **Gas.** The driver has the tx hash but not the gas. BYOS makes one `eth_getTransactionReceipt` call to read the real gas used and gas price. No block scanning, no subscriptions.
- **Escrow stays native.** It applies `gas + c_l` for Track A and handles Track B. Track B (freeze/debit on a CoW ruling) is triggered by hand, not by a chain event, so it cannot live in an event-driven watcher anyway.

## Alternatives considered

- **Chain watcher (scan blocks).** Rejected as the primary source. Rebuilds what the driver does, misses private and dropped txs, and cannot spot a missed deadline without knowing what we sent. It becomes the fallback only if CoW declines to run our fork.
- **Autopilot observations.** Rejected. CoW-run, tied to its database, wrong attribution level, gas table dropped (migration `V090`). Useful as a reference for the receipt read, not as a source.
- **Shepherd (WASM) module.** Rejected here. Shepherd earns its place when you subscribe to chain events. We have a push plus one read, so there is nothing to subscribe to. Still a fit for event-driven jobs like TWAP or EthFlow.
- **Driver applies the penalty.** Rejected. Keys and the debit decision stay in our audited escrow code, and the driver cannot do Track B.

## Consequences

- No chain indexer to build or run.
- We depend on CoW running our driver fork with the outcome hook. That is the one external dependency here: CoW has signaled openness but not confirmed. If they say no, we revisit this ADR — the fallback is a receipt-polling watcher over the txs we know we proposed, which is worse (misses private/dropped txs and deadline detection) but workable.
- Missed-deadline detection is free, from the driver's `Expired` outcome.
- Private submissions are covered, because the driver is the source.
- The receipt read is required, not a double-check: it is how we get the gas to debit.
- The "won the auction but chose not to settle" Track A case is out of scope here. It has no tx and comes from our own auction records.
- We are not using Shepherd for this. Written down so we do not revisit it.
