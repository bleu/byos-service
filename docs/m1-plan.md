# M1 Plan: Tasks and Decisions

## Task 1: Wire RPC provider and implement real proposal validator

**Linear refs:** COW-1166 (ingestion gatekeeping), COW-1162 (simulation)

### Context

The service currently has zero chain connectivity. The `ProposalValidator` is an `AcceptAll` stub. To test contracts integration in M1, the service must connect to the chain and perform real escrow balance checks and settlement simulation.

### Scope

#### 1. Add RPC provider to the service

- Add `--rpc-url` CLI arg to `run.rs` (clap derive, env var `RPC_URL`)
- Create an alloy HTTP provider at boot, inject into `AppState`
- Fail-fast if RPC is unreachable at startup

#### 2. Implement escrow balance check

- Call `Escrow.effectiveBalance(sub_solver)` via the provider
- Compare against minimum: `gas + c_l`
  - `c_l` is chain-specific (Mainnet: 0.010 ETH, Gnosis: 10 xDAI) — read from config
  - `gas` uses the same fixed 200k gas estimate currently in `/solve`
- Cache escrow balances per sub-solver with a short TTL (e.g., 1 block / 12s) to avoid re-reading every validation tick for the same signer
- Reject with `RejectionReason::InsufficientEscrow` if below minimum
- Note: a passing escrow check implicitly proves the trampoline is deployed (Escrow.deposit calls TrampolineFactory.ensureDeployed — contracts ADR-0003)

#### 3. Implement simulation via eth_call

- Use the existing calldata builder in `infra/blockchain/simulation.rs` (`build_simulation_calldata()`)
- Send the built calldata via `eth_call` to the provider (target: GPv2Settlement address, from: settlement address)
- If the call reverts, verdict is `SimFailed`
- If the call succeeds, verdict is `Accept` (proposal becomes `Active`)
- GPv2Settlement address needs to be added as a config param (CLI arg / env var)

#### 4. Replace AcceptAll with the real validator

- Implement `ProposalValidator` trait with a struct that holds the provider + escrow cache + config
- Validation order: escrow check first (cheap cached read), then simulation (expensive eth_call)
- Wire the new validator into the background validation loop (replace `AcceptAll` in `run.rs`)

#### 5. Resolve trampoline address in /solve

- Replace `Address::ZERO` in `solve.rs:96` with the CREATE2 computation from `byos-common/trampoline.rs`
- The sub-solver address is already recovered from the proposal signature and stored — use it as the CREATE2 salt
- No RPC needed (pure local computation), deployment guaranteed by escrow check at ingestion

#### 6. Use simulation gas in /solve scoring

- Replace the fixed `M1_GAS_ESTIMATE = 200_000` in `solve.rs` with the actual gas consumed by the simulation `eth_call` (from Task 1.3) plus a 100,000 gas buffer
- Store the simulation gas result on the proposal at ingestion time so `/solve` can read it without re-simulating
- Scoring formula becomes: `gas = simulated_gas + 100_000`

### Out of scope (M2)

- Continuous re-simulation (periodic re-check of Active proposals every 3-5 blocks)
- EBBO baseline check
- Fee adequacy gate (fee_rate = 0 in v1, always passes)
- Order liveness check against CoW orderbook
- Amount sanity validation
- Rate limiting (IP + signer-based, escrow-tiered)
- Track B 5x reserve tracking against pending claims

### Dependencies

- Escrow contract address (config)
- GPv2Settlement contract address (config)
- TrampolineFactory contract address (already a CLI arg: `--trampoline-factory`)
- `c_l` value per chain (config — from ADR-0003)

### Test plan

- Unit test: validator rejects when escrow balance < gas + c_l
- Unit test: validator accepts when escrow balance >= gas + c_l
- Unit test: validator returns SimFailed when eth_call reverts
- Unit test: validator returns Accept when eth_call succeeds
- Unit test: escrow cache returns cached value within TTL, re-fetches after TTL
- Service-level test: full proposal submission flow with real escrow check against anvil
- Service-level test: proposal rejected due to insufficient escrow against anvil
