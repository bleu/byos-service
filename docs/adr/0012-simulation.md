# Simulation

Status: accepted

## Context

This ADR settles how proposals are simulated, how the gas result flows into scoring, and the continuous re-validation of active proposals.

Depends on: [ADR-0001](0001-proposal-api.md) (proposal lifecycle), [ADR-0002](0002-solver-engine.md) (solver engine scoring).

## Decision

### Simulation dispatch: trampoline code override on the user's address

Each proposal is simulated by overriding the **user's** (order owner's) address with the trampoline's deployed bytecode, then calling `Trampoline.execute()` from the settlement contract:

```
eth_estimateGas:
  from: settlement
  to:   user (code overridden with trampoline bytecode)
  data: Trampoline.execute(proposal, interactions, buyToken, signature)
```

This works because:

1. **`msg.sender == SETTLEMENT`**: the settlement is `from`, so the trampoline's `msg.sender` check passes without modifying the trampoline code.
2. **The user already holds the sell tokens**: the trampoline code at the user's address can spend them directly — no transfer step, no approval override, no balance-slot detection.
3. **Trampoline immutables are correct**: we copy the real trampoline's deployed bytecode (via `eth_getCode`), which has `SUB_SOLVER`, `SETTLEMENT`, `DOMAIN_SEPARATOR`, and `ESCROW` already embedded for that sub-solver.

Two state overrides are applied:

- **User address**: `code` override with the trampoline's deployed bytecode.
- **Escrow address**: `state_diff` override granting `SUBMITTER_ROLE` to the settlement address (`tx.origin`). The trampoline checks `escrow.hasRole(SUBMITTER_ROLE, tx.origin)` to prevent unauthorized replay. The storage slot is computed from OpenZeppelin v5's ERC-7201 namespaced layout (deterministic, stable).

Using `eth_estimateGas` (rather than `eth_call` + a separate gas estimation step) gives both the success/revert verdict and the gas consumed in a single RPC call. A successful estimate means the proposal would settle; a revert means it would fail.

### Required proposal fields: `sellToken` and `buyToken`

`POST /proposals` requires two additional fields: `sellToken` and `buyToken` (the order's token addresses). These are needed to build the simulation calldata. The sub-solver provides them; in the future they may be fetched from the Orderbook API instead.

### Trampoline resolution: `TrampolineFactory.addressOf` + `eth_getCode` at validation time

The simulation needs a trampoline address and its deployed bytecode. Both are resolved during the first validation pass (`Submitted` -> `Active`):

1. `TrampolineFactory.addressOf(sub_solver)` returns the trampoline address.
2. `eth_getCode(trampoline)` returns the deployed bytecode (including immutables).

Results are cached per sub-solver in a `HashMap<Address, TrampolineInfo>` — trampoline addresses are deterministic (CREATE2) and their bytecode never changes, so the cache is persistent across ticks.

The resolved trampoline address is stored on the `Proposal` struct and used by both re-validation (no re-resolution needed) and `/solve` (for encoding settlement interactions).

### Gas in scoring: simulated gas + 100k buffer

The simulated gas is stored on the `Proposal` struct (`gas_used: Option<u64>`) after a successful simulation. `/solve` scoring uses `gas = gas_used + GAS_BUFFER` where `GAS_BUFFER = 100_000`. The buffer accounts for node-level estimation overhead and minor state differences between simulation and settlement.

Proposals without `gas_used` (not yet simulated) are skipped by `/solve` -- they are never scored or returned as solutions.

The old fixed `GAS_ESTIMATE` constant is retained only for the escrow balance threshold calculation (renamed to `ESCROW_GAS_ESTIMATION`), which runs before simulation and needs a conservative floor.

### Continuous re-validation of active proposals

The background validation loop validates both `Submitted` and `Active` proposals on every tick. For `Submitted` proposals, a successful validation transitions them to `Active` and writes `gas_used` and `trampoline`. For `Active` proposals, re-validation updates `gas_used` with the fresh simulation result; if the simulation now reverts, the proposal transitions to `SimFailed`.

This catches proposals that become invalid due to on-chain state changes (pool liquidity moved, user balance changed, etc.) without waiting for the driver's post-encoding re-simulation.

### Error handling: defer on transport errors, fail on reverts

- **Simulation reverts** (the EVM executed the call and it failed): verdict is `SimFailed`, the proposal is permanently dropped.
- **Transport errors** (RPC timeout, DNS failure, connection refused): verdict is deferred (`None`), the proposal stays in its current state and is retried next tick. A broken RPC should not punish sub-solvers.
- **Trampoline resolution errors**: same deferral policy -- transport errors defer, server errors (contract revert) are treated as real failures.

### Validator architecture: `ValidateProposal` trait + composite `ProposalValidator`

The `ProposalValidator` trait is renamed to `ValidateProposal` (verb form, idiomatic Rust for traits). The composite struct takes the name `ProposalValidator` -- it holds an `EscrowValidator` and a `SimulationValidator` and runs them in sequence:

1. Escrow check (cheap, per-tick cached balance read)
2. Simulation (expensive, `eth_estimateGas` RPC call)

Short-circuits on the first non-`Accept` verdict. The `Verdict::Accept` variant carries `gas_used: Option<u64>` and `trampoline: Option<Address>`, which the store writes onto the proposal.

### Configuration: `--settlement-address`

A new CLI arg `--settlement-address` (env: `SETTLEMENT_ADDRESS`) is required when `--rpc-url` is set. It specifies the GPv2Settlement contract address, used as the `from` address for simulation calls.

## Alternatives rejected

### A. Settlement calls itself with a fake `settle()`

Build a minimal `settle()` calldata with empty trades and three intra-interactions: `transferFrom(user, settlement, sellAmount)`, `transfer(trampoline, sellAmount)`, `trampoline.execute(...)`. The settlement is both `from` and `to`.

**Rejected because:** the `transferFrom` step requires the user to have approved the settlement contract. In CoW Protocol, users approve the **VaultRelayer**, not the settlement — so this simulation would revert on real mainnet state.

### B. Settlement code override with a simulation harness

Override the settlement contract's code with a compiled harness that calls `transferFrom(msg.sender, trampoline, sellAmount)` then `trampoline.execute(...)`. The user calls the harness at the settlement address.

**Rejected because:** it has the same approval problem as alternative A. The harness at the settlement address calls `transferFrom` where the ERC-20 checks `allowance[user][settlement]`, which doesn't exist — users approved the VaultRelayer.

### C. Multicall with trampoline code override

Use Multicall3 to batch `sellToken.transfer(trampoline, sellAmount)` and `trampoline.execute(...)`. Override the trampoline's bytecode to change the immutable `SETTLEMENT` to the Multicall3 address.

**Rejected because:** Multicall3 uses `call` (not `delegatecall`), so `msg.sender` for each subcall is the Multicall3 contract. The `sellToken.transfer()` would need Multicall3 to hold the sell tokens, which requires ERC-20 balance-slot detection — the same fragile, token-specific probing we wanted to avoid. `delegatecall` doesn't help either: it preserves `msg.sender` but changes the storage context to the caller's, so ERC-20 balance reads return wrong values.

### D. ERC-20 balance state overrides on the trampoline

Call `trampoline.execute()` directly from the settlement, giving the trampoline sell tokens via `state_diff` overrides on the ERC-20's `balanceOf` storage slot.

**Rejected because:** detecting the correct storage slot for an arbitrary ERC-20's `balanceOf` mapping is fragile and requires ongoing maintenance. Different tokens use different slot layouts (Solidity mapping slots 0–10, Solady packed layout, proxy patterns, etc.). Tokens with non-standard layouts would either be incorrectly simulated or rejected as unsupported. The chosen approach avoids this entirely by using the user's real token balance.

## Consequences

- **`POST /proposals` has two new required fields.** Existing sub-solver clients must send `sellToken` and `buyToken`. The reference `subsolver` crate must be updated.
- **Proposals without simulation are invisible to `/solve`.** In `AcceptAll` mode (no RPC), `/solve` returns empty solutions. This is correct -- without chain connectivity, proposals cannot be meaningfully scored or settled.
- **RPC load scales with active proposals.** Every active proposal is re-simulated on every tick. The trampoline cache mitigates one call per sub-solver (both address and bytecode), but `eth_estimateGas` runs every tick for every live proposal. Acceptable for the expected M1 proposal volume.
- **Token-agnostic.** The simulation works with any ERC-20 token — no balance-slot detection or per-token probing is needed. The user's real token balance is used directly.
- **Interaction recipient caveat.** If a sub-solver's interaction calldata hardcodes the real trampoline address as the swap output recipient, those tokens go to the real trampoline instead of the user-acting-as-trampoline, causing the settle-back to revert. Sub-solvers should design interactions so that output tokens land at the executing contract's address (i.e., the address running the trampoline code).
- **Anvil integration tests** are deferred to COW-1165. Unit tests use mock providers (unreachable RPC) to verify error classification and deferral behavior.
