# Simulation

Status: accepted

## Context

This ADR settles how proposals are simulated, how the gas result flows into scoring, and the continuous re-validation of active proposals.

Depends on: [ADR-0001](0001-proposal-api.md) (proposal lifecycle), [ADR-0002](0002-solver-engine.md) (solver engine scoring).

## Decision

### Simulation dispatch: `eth_estimateGas` on `trampoline.execute()` with balance override

Each proposal is simulated by calling `trampoline.execute(proposal, interactions, buyToken, signature)` via `eth_estimateGas`. The call is sent `from: settlement` (so the Trampoline's `OnlySettlement` guard passes) and `to: trampoline`.

A **balance state override** gives the trampoline the sell tokens it needs: `balanceOf[trampoline] = sellAmount` on the sell token contract. This avoids the need to simulate a full `settle()` (which would require order data only available at `/solve` time) and sidesteps the allowance problem (users approve the VaultRelayer, not the settlement contract).

The balance slot is detected via heuristic probing at first encounter of each sell token:

- **Solidity mapping:** Probe slot indices 0..10 — compute `keccak256(pad32(holder) ++ pad32(slot_index))`, write a sentinel value via `eth_call` state override, read back `balanceOf(holder)`.
- **Solady mapping:** Compute `keccak256(holder[0..20] ++ 0x00000000_87a211a2)`, same sentinel verification.
- Results are cached per token (storage layouts are immutable). If detection fails, the proposal is rejected with `UnsupportedToken`.

Using `eth_estimateGas` (rather than `eth_call` + a separate gas estimation step) gives both the success/revert verdict and the gas consumed in a single RPC call. A successful estimate means the sub-solver's route executes and produces `buyAmount` of buy tokens; a revert means it would fail.

### Required proposal fields: `sellToken` and `buyToken`

`POST /proposals` requires two additional fields: `sellToken` and `buyToken` (the order's token addresses). These are needed to build the simulation calldata. The sub-solver provides them; in the future they may be fetched from the Orderbook API instead.

### Trampoline resolution: `TrampolineFactory.addressOf` at validation time

The simulation needs a trampoline address. It is resolved by calling `TrampolineFactory.addressOf(sub_solver)` on-chain during the first validation pass (`Submitted` -> `Active`). Results are cached per sub-solver in a `HashMap<Address, Address>` -- trampoline addresses are deterministic (CREATE2) and never change, so the cache is persistent across ticks.

The resolved trampoline is stored on the `Proposal` struct and used by both re-validation (no re-resolution needed) and `/solve` (for encoding settlement interactions).

### Gas in scoring: simulated gas + 100k buffer

The simulated gas is stored on the `Proposal` struct (`gas_used: Option<u64>`) after a successful simulation. `/solve` scoring uses `gas = gas_used + GAS_BUFFER` where `GAS_BUFFER = 100_000`. The buffer accounts for node-level estimation overhead and minor state differences between simulation and settlement.

Proposals without `gas_used` (not yet simulated) are skipped by `/solve` -- they are never scored or returned as solutions.

The old fixed `GAS_ESTIMATE` constant is retained only for the escrow balance threshold calculation (renamed to `ESCROW_GAS_ESTIMATION`), which runs before simulation and needs a conservative floor.

### Continuous re-validation of active proposals

The background validation loop validates both `Submitted` and `Active` proposals on every tick. For `Submitted` proposals, a successful validation transitions them to `Active` and writes `gas_used` and `trampoline`. For `Active` proposals, re-validation updates `gas_used` with the fresh simulation result; if the simulation now reverts, the proposal transitions to `SimFailed`.

This catches proposals that become invalid due to on-chain state changes (pool liquidity moved, user balance changed, etc.) without waiting for the driver's post-encoding re-simulation.

### Error handling: defer on transport errors, fail on reverts

- **Simulation reverts** (the EVM executed the call and it failed, error code 3): verdict is `SimFailed`, the proposal is permanently dropped.
- **Transport errors** (RPC timeout, DNS failure, connection refused) and non-revert RPC errors (rate limiting, gas caps): verdict is deferred (`None`), the proposal stays in its current state and is retried next tick. A broken RPC should not punish sub-solvers.
- **Trampoline resolution errors**: same deferral policy -- transport errors defer, server errors (contract revert) are treated as real failures.
- **Unsupported token** (balance slot detection failed): verdict is `Reject(UnsupportedToken)`. The token's storage layout is not a standard Solidity or Solady mapping. Cached permanently per token.

### Validator architecture: `ValidateProposal` trait + composite `ProposalValidator`

The `ProposalValidator` trait is renamed to `ValidateProposal` (verb form, idiomatic Rust for traits). The composite struct takes the name `ProposalValidator` -- it holds an `EscrowValidator` and a `SimulationValidator` and runs them in sequence:

1. Escrow check (cheap, per-tick cached balance read)
2. Simulation (expensive, `eth_estimateGas` RPC call)

Short-circuits on the first non-`Accept` verdict. The `Verdict::Accept` variant carries `gas_used: Option<u64>` and `trampoline: Option<Address>`, which the store writes onto the proposal.

### Configuration: `--settlement-address`

A new CLI arg `--settlement-address` (env: `SETTLEMENT_ADDRESS`) is required when `--rpc-url` is set. It specifies the GPv2Settlement contract address, used as both `from` and `to` for simulation calls.

## Consequences

- **`POST /proposals` has two new required fields.** Existing sub-solver clients must send `sellToken` and `buyToken`. The reference `subsolver` crate must be updated.
- **Proposals without simulation are invisible to `/solve`.** In `AcceptAll` mode (no RPC), `/solve` returns empty solutions. This is correct -- without chain connectivity, proposals cannot be meaningfully scored or settled.
- **RPC load scales with active proposals.** Every active proposal is re-simulated on every tick. The trampoline cache mitigates one call per sub-solver, but `eth_estimateGas` runs every tick for every live proposal. Acceptable for the expected M1 proposal volume.
- **Anvil integration tests** are deferred to COW-1165. Unit tests use mock providers (unreachable RPC) to verify error classification and deferral behavior.
