# Simulation

Status: accepted

## Context

This ADR settles how proposals are simulated, how the gas result flows into scoring, and the continuous re-validation of active proposals.

Depends on: [ADR-0001](0001-proposal-api.md) (proposal lifecycle), [ADR-0002](0002-solver-engine.md) (solver engine scoring).

The design was proven end-to-end by a mainnet fork spike (COW-1181, 2026-07-27): a real orderbook order was settled through a freshly deployed trampoline from an unprivileged sender using only the two state overrides below.

## Decision

### Simulation dispatch: a full one-trade `settle()` from a dummy submitter

Each proposal is simulated as the transaction the CoW-run driver would actually submit — a real `settle()` on GPv2Settlement:

```
eth_estimateGas:
  from: 0x1111...1111 (dummy submitter)
  to:   GPv2Settlement
  data: settle(
          tokens         = [sellToken, buyToken],
          clearingPrices = [proposal.buyAmount, proposal.sellAmount],
          trades         = [the real order: fields and signature from the orderbook],
          interactions   = [[], [sellToken.transfer(trampoline, sellAmount),
                                 trampoline.execute(...)], []]
        )
state overrides:
  authenticator -> code: AnyoneAuthenticator (vendored from cowprotocol/services, MIT)
  escrow        -> state_diff: hasRole(SUBMITTER_ROLE, dummy) = true
```

Because the order is real, the user has genuinely approved the vault relayer and holds the sell tokens — no balance faking, no allowance faking, no per-token storage-slot detection. Everything runs at real addresses, so the trampoline's floor-and-sweep semantics (contracts ADR-0003) behave exactly as production, and GPv2's own checks (order signature, limit price, validTo, filled amount) come along for free.

Using `eth_estimateGas` (rather than `eth_call` + a separate gas estimation step) gives both the success/revert verdict and the gas consumed in a single RPC call.

The two overrides stand in for permissions the dummy sender lacks:

- **Authenticator** (resolved once via `settlement.authenticator()`, cached): code override with `AnyoneAuthenticator`, so any `from` passes the solver allowlist.
- **Escrow**: a `state_diff` write granting `SUBMITTER_ROLE` to the dummy (`tx.origin` check in the trampoline). The Escrow inherits **non-upgradeable** OpenZeppelin v5 `AccessControl`, so `_roles` lives at plain storage slot 5 (after ERC20's five slots): the slot is `keccak256(pad32(account) ++ keccak256(pad32(role) ++ pad32(5)))`. This is NOT the ERC-7201 namespaced slot the upgradeable variant uses — an earlier draft assumed it and every simulation reverted on the fork.

Once a production submitter address holds `SUBMITTER_ROLE` on-chain, a `--submitter-address` arg can replace the dummy and both overrides become unnecessary.

### Order data: fetched from the orderbook, cached forever

The order (amounts, validTo, appData, receiver, signature, signing scheme, balance flavors, fullAppData) is fetched from `GET /api/v1/orders/{uid}` on the CoW orderbook (`--orderbook-url`, required alongside `--rpc-url`). `POST /proposals` does NOT carry token addresses — the orderbook order is the single source of truth, which removes a lying-client hazard.

Orders are immutable once placed, so fetches are cached for the process lifetime. Known staleness: an off-chain soft-cancel is invisible to this service — the proposal's own `validUntil` bounds the window, and the driver re-validates orders at settlement time, so nothing wrong can land on-chain. Fetch failures: 404 rejects the proposal (`OrderNotFound`); anything transient defers to the next tick.

### Validation envelope: which orders get simulated at all

Before simulating, the order/proposal pair must pass the envelope (cheap, no RPC):

- **Fill-or-kill only** — partially fillable orders reject (`UnsupportedOrder`).
- **No hooks** — orders whose `fullAppData` declares `metadata.hooks` (or `metadata.bridging`, which implies hooks) reject (`UnsupportedOrder`). The check must parse fullAppData: essentially every real order has a non-zero `appData` hash (appCode/referrer metadata), so filtering on `appData == 0` would reject the whole orderbook. Hook support is a capability gap tracked in COW-1197.
- **erc20 balance flavors only** — `external`/`internal` balance orders reject (`UnsupportedOrder`).
- **Amounts consistent** — sell fill-or-kill needs `proposal.sellAmount == order.sellAmount`; buy needs `proposal.buyAmount == order.buyAmount` (`AmountMismatch`). Fill-or-kill executes the order amount in full; a proposal quoting anything else would simulate a different trade than the one the driver settles.

All four signature schemes (eip712, ethsign, eip1271, presign) are supported: the scheme is encoded into the trade's `flags` word and GPv2 verifies the signature for real during the simulation. Sell and buy orders are both supported, including native-ETH buys.

### Trampoline resolution: `TrampolineFactory.addressOf` at validation time

The simulation needs a trampoline address. It is resolved by calling `TrampolineFactory.addressOf(sub_solver)` on-chain during the first validation pass (`Submitted` -> `Active`). Results are cached per sub-solver in a `HashMap<Address, Address>` -- trampoline addresses are deterministic (CREATE2) and never change, so the cache is persistent across ticks.

The resolved trampoline is stored on the `Proposal` struct and used by both re-validation (no re-resolution needed) and `/solve` (for encoding settlement interactions). The order's token addresses are stored on the proposal the same way (via the `Accept` verdict) for `/solve` scoring and encoding.

### Gas in scoring: simulated gas + 30k buffer

The simulated gas is stored on the `Proposal` struct (`gas_used: Option<u64>`) after a successful simulation. `/solve` scoring uses `gas = gas_used + GAS_BUFFER` where `GAS_BUFFER = 30_000`. The buffer is small by design: the full-settle estimate already covers intrinsic gas and the entire settlement path, so it only absorbs warm/cold storage differences and driver batching variance. (The old 100k buffer compensated for a simulation that under-measured the real transaction.)

Proposals without `gas_used` (not yet simulated) are skipped by `/solve` -- they are never scored or returned as solutions.

The old fixed `GAS_ESTIMATE` constant is retained only for the escrow balance threshold calculation (renamed to `ESCROW_GAS_ESTIMATION`), which runs before simulation and needs a conservative floor.

### Continuous re-validation of active proposals

The background validation loop validates both `Submitted` and `Active` proposals on every tick. For `Submitted` proposals, a successful validation transitions them to `Active` and writes `gas_used`, `trampoline`, and the order's token addresses. For `Active` proposals, re-validation updates `gas_used` with the fresh simulation result; if the simulation now reverts, the proposal transitions to `SimFailed`.

This catches proposals that become invalid due to on-chain state changes (pool liquidity moved, user balance changed, order filled or invalidated on-chain, etc.) without waiting for the driver's post-encoding re-simulation.

### Error handling: defer on transport errors, fail on reverts

- **Simulation reverts** (the EVM executed the call and it failed): verdict is `SimFailed`, the proposal is permanently dropped.
- **Transport errors** (RPC timeout, DNS failure, connection refused): verdict is deferred (`None`), the proposal stays in its current state and is retried next tick. A broken RPC should not punish sub-solvers.
- **Trampoline resolution errors**: same deferral policy -- transport errors defer, server errors (contract revert) are treated as real failures.
- **Orderbook errors**: 404 rejects (`OrderNotFound`); transient errors defer.

### Validator architecture: `ValidateProposal` trait + composite `ProposalValidator`

The `ProposalValidator` trait is renamed to `ValidateProposal` (verb form, idiomatic Rust for traits). The composite struct takes the name `ProposalValidator` -- it holds an `EscrowValidator` and a `SimulationValidator` and runs them in sequence:

1. Escrow check (cheap, per-tick cached balance read)
2. Simulation (orderbook fetch — cached after the first pass — then envelope check, then the `eth_estimateGas` RPC call)

Short-circuits on the first non-`Accept` verdict. The `Verdict::Accept` variant carries `gas_used: Option<u64>`, `trampoline: Option<Address>`, and `tokens: Option<(Address, Address)>`, which the store writes onto the proposal.

### Configuration: `--settlement-address`, `--orderbook-url`

`--settlement-address` (env: `SETTLEMENT_ADDRESS`) is the GPv2Settlement contract address, the simulation's `to`. `--orderbook-url` (env: `ORDERBOOK_URL`) is the CoW orderbook base URL (e.g. `https://api.cow.fi/mainnet`). Both are required when `--rpc-url` is set.

## Alternatives rejected

### A. Settlement calls itself with a fake `settle()`

Empty trades plus three intra-interactions, the first being `transferFrom(user, settlement, sellAmount)`. Rejected: users approve the **VaultRelayer**, not the settlement, so the `transferFrom` reverts on real mainnet state for every real order.

### B. ERC-20 balance state overrides on the trampoline

Call `trampoline.execute()` directly, faking the trampoline's sell-token balance via `state_diff` on the token's `balanceOf` slot. Rejected: detecting the balance slot of an arbitrary ERC-20 is fragile per-token probing with ongoing maintenance (different Solidity layouts, Solady, proxies). CoW's own quote verifier needs an entire crate plus `debug_traceCall` for this.

### C. Trampoline code override at the user's address

Copy the trampoline's bytecode onto the user's address and call `execute()` from the settlement; the user's real balance funds the route. Rejected: under the contracts' floor-and-sweep semantics (byos-contracts #27), "the instance" would be the user, so the sweep drags the user's pre-existing buy-token balance into the floor delta — a user who already holds buy tokens makes a worthless route pass. Also failed to exercise the vault-relayer pull and mis-simulated routes that hardcode the real trampoline address.

### D. `simulateExecute()` on the trampoline

A revert-at-the-end simulation entrypoint in the trampoline bytecode (Uniswap Quoter pattern). Workable, but strictly worse than the full settle: new audit surface on the contracts, a synthetic call shape instead of the real settlement, no vault-relayer or GPv2-checks coverage, and `eth_call` + revert-data decoding instead of a single `eth_estimateGas`.

## Consequences

- **`POST /proposals` carries no token addresses.** The orderbook is the source of truth; the reference `subsolver` client already matches this contract.
- **The orderbook is a runtime dependency of validation.** If it is down, proposals defer (they are not rejected). One fetch per order uid over the process lifetime.
- **Proposals without simulation are invisible to `/solve`.** In `AcceptAll` mode (no RPC), `/solve` returns empty solutions. This is correct -- without chain connectivity, proposals cannot be meaningfully scored or settled.
- **RPC load scales with active proposals.** Every active proposal is re-simulated on every tick. The trampoline and authenticator caches mitigate the lookups, but `eth_estimateGas` runs every tick for every live proposal. Acceptable for the expected M1 proposal volume.
- **Hooked orders are rejected, not settled.** A scope cut, not a technical wall: the pre/post interaction slots in the encoder are where driver-matching hook interactions would go (COW-1197).
- **Anvil integration tests** are deferred to COW-1165; the fork spike test (`SpikeRealOrder.t.sol`, attached to COW-1181) seeds that work. Unit tests pin the `settle()` encoding byte-for-byte against Solidity's `abi.encodeCall` for a real mainnet order, and a fake-RPC test pins the wire shape of the estimate request (sender, calldata, both overrides).
