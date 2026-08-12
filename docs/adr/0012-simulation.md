# Simulation

Status: accepted

Spec: docs/shared/design-document.md#simulation
      https://bleu.github.io/byos-docs/design-document#simulation

## Context

This ADR settles how proposals are simulated, how the gas result flows into scoring, and the continuous re-validation of active proposals.

Depends on: [ADR-0001](0001-proposal-api.md) (proposal lifecycle), [ADR-0002](0002-solver-engine.md) (solver engine scoring).

The design was proven end-to-end by a mainnet fork spike (COW-1181, 2026-07-27): a real orderbook order was settled through a freshly deployed trampoline from an unprivileged sender using only the two state overrides below.

## Decision

The simulation dispatch shape, state overrides, validation envelope checks, first-revert-terminal rule, profitability gate, and re-simulation interval are specified in the design document (see Spec link above). This ADR records the implementation-specific reasoning.

### Simulation dispatch: a full one-trade `settle()` from a dummy submitter

Because the order is real, the user has genuinely approved the vault relayer and holds the sell tokens — no balance faking, no allowance faking, no per-token storage-slot detection. Using `eth_estimateGas` (rather than `eth_call` + a separate gas estimation step) gives both the success/revert verdict and the gas consumed in a single RPC call.

**What the simulation does not model.** `byos_common::settlement::encode_settle` sets `executedAmount` to the proposal's fill amount and `clearingPrices` to the raw proposal amounts. The driver's real transaction subtracts the gas cut and substitutes its own per-trade prices, then applies protocol fees. Neither is threaded through the encoder, for two reasons. The gas is the same (same tokens, interactions, trade, storage touched). And the divergence is one-directional — the real transaction pays the user less than the simulated one, never more — so a proposal that simulates successfully cannot fail the settlement's limit check because of the cut.

The two overrides stand in for permissions the dummy sender lacks:

- **Authenticator** (resolved once via `settlement.authenticator()`, cached): code override with `AnyoneAuthenticator`.
- **Escrow**: a `state_diff` write granting `SUBMITTER_ROLE` to the dummy. The Escrow inherits **non-upgradeable** OpenZeppelin v5 `AccessControl`, so `_roles` lives at plain storage slot 5 (after ERC20's five slots): the slot is `keccak256(pad32(account) ++ keccak256(pad32(role) ++ pad32(5)))`. This is NOT the ERC-7201 namespaced slot the upgradeable variant uses.

Once a production submitter address holds `SUBMITTER_ROLE` on-chain, `--submitter-address` replaces the dummy and both overrides become unnecessary.

### Order data: fetched from the orderbook, cached forever

Orders are immutable once placed, so fetches are cached for the process lifetime. Known staleness: an off-chain soft-cancel is invisible to this service — the proposal's `validUntil` bounds the window, and the driver re-validates at settlement time.

### Hooks + partial fills interaction

The CoW social consensus is that pre-hooks execute only on the first fill (`executed_amount == 0`), while post-hooks execute on every fill. Because orders are cached by UID for the process lifetime, pre-interactions are always present in simulation regardless of fill history. This overestimates gas for non-first fills of hooked orders (conservative: never under-counts, but reduces surplus slightly). See the `SimulationParams` doc comments for detail.

### Trampoline resolution: `TrampolineFactory.addressOf` at validation time

Results are cached per sub-solver in a `HashMap<Address, Address>` — trampoline addresses are deterministic (CREATE2) and never change. The resolved trampoline is stored on the `Proposal` struct and used by both re-validation and `/solve`.

### Gas in scoring: simulated gas + 30k buffer

`GAS_BUFFER = 30_000`. The buffer is small by design: the full-settle estimate already covers intrinsic gas and the entire settlement path. The old fixed `GAS_ESTIMATE` constant is retained only for the escrow balance threshold calculation (renamed to `ESCROW_GAS_ESTIMATION`).

### Error handling: defer on transport errors, fail on reverts

- **Simulation reverts**: verdict is `SimFailed`, the proposal is permanently dropped.
- **Transport errors**: deferred to the next tick. A broken RPC should not punish sub-solvers.
- **Trampoline resolution errors**: same deferral policy.
- **Orderbook errors**: 404 rejects (`OrderNotFound`); transient errors defer.

### Validator architecture: `ValidateProposal` trait + composite `ProposalValidator`

The composite struct holds an `EscrowValidator` and a `SimulationValidator`, runs them in sequence, and short-circuits on the first non-`Accept` verdict. The `Verdict::Accept` variant carries `gas_used`, `trampoline`, and `tokens`, which the store writes onto the proposal.

### Configuration: `--settlement-address`, `--orderbook-url`

`--settlement-address` (env: `SETTLEMENT_ADDRESS`) is the GPv2Settlement contract address. `--orderbook-url` (env: `ORDERBOOK_URL`) is the CoW orderbook base URL. Both are required when `--rpc-url` is set.

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
- **RPC load scales with active proposals.** Every active proposal is re-simulated on every tick. The trampoline and authenticator caches mitigate the lookups, but `eth_estimateGas` runs every tick for every live proposal. Acceptable for the expected M1 proposal volume — and bounded per proposal by the ingestion lifetime cap ([ADR-0013](0013-proposal-lifecycle-and-retention.md)).
- **Hooked orders are supported** (COW-1243). The orderbook pre-encodes hooks as `HooksTrampoline.execute()` calls in the order's `interactions` field. These are included in simulation for accurate gas estimates but NOT returned by `/solve` — the driver appends the order's own hooks itself. This includes partially fillable orders with hooks: the envelope accepts both features, and settlement encoding handles `executedAmount` (partial fill fraction) and hook interactions (`interactions[0]`/`[2]`) independently. **Known trade-off:** gas is overestimated for non-first fills because the order cache always includes pre-interactions; fixing this would require tracking `executedAmount` and invalidating the cache.
- **Anvil integration tests** are deferred to COW-1165; the fork spike test (`SpikeRealOrder.t.sol`, attached to COW-1181) seeds that work. Unit tests pin the `settle()` encoding byte-for-byte against Solidity's `abi.encodeCall` for a real mainnet order, and a fake-RPC test pins the wire shape of the estimate request (sender, calldata, both overrides).
