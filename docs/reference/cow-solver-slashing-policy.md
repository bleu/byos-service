# CoW Protocol Solver Penalty & Slashing Framework

There are **three enforcement layers**: smart contract (on-chain), automated off-chain (autopilot/driver code), and governance (DAO social consensus).

---

## Layer 1: Smart Contract Enforcement (On-Chain, Automatic)

Hard-coded in the settlement contract and cannot be bypassed.

| Rule | What Happens |
|------|-------------|
| **Limit price violation** | Transaction reverts — orders cannot execute at prices worse than the user's limit |
| **Solver not whitelisted** | Transaction reverts — only bonded, approved solvers can submit |

---

## Layer 2: Automated Off-Chain Enforcement (Autopilot + Driver Code)

Enforced programmatically by the autopilot and driver services.

### 2a. Participation Guard — Solver Banning (Autopilot)

| Policy | Trigger | Default Threshold | Penalty | Duration |
|--------|---------|-------------------|---------|----------|
| **Non-settling** | Won N consecutive auctions, settled none | 3 consecutive unsettled wins | Auction ban | 5 min |
| **Low-settling** | Settlement failure rate too high across window | >90% failure over 100 auctions (min 3 wins) | Auction ban | 5 min |

Both are configurable and enabled by default. Banned solvers receive HTTP notifications (`Banned { reason, until }`).

**Configuration parameters:**

| Parameter | Default | Purpose |
|-----------|---------|---------|
| `db_enabled` | true | Master switch for participation guard |
| `solver_blacklist_cache_ttl` | 5m | How long bans last |
| `non_settling_solvers_blacklisting_enabled` | true | Enable non-settling policy |
| `non_settling_last_auctions_participation_count` | 3 | Auction window for non-settling policy |
| `low_settling_solvers_blacklisting_enabled` | true | Enable low-settling policy |
| `low_settling_last_auctions_participation_count` | 100 | Auction window for low-settling policy |
| `low_settling_min_wins_threshold` | 3 | Min wins before evaluation |
| `solver_max_settlement_failure_rate` | 0.9 | Max acceptable failure rate (90%) |

### 2b. Settlement Validation (Driver)

Before any settlement hits the chain, the driver validates:

- **Trusted token check** — internalized interactions can only use trusted tokens; violation produces `NonBufferableTokensUsed` error and settlement is blocked
- **Simulation** — settlement must not revert in simulation
- **Gas safety** — sufficient gas parameters required

### 2c. Settlement Deadline Enforcement

Settlements must land within chain-specific block deadlines:

| Chain | Deadline |
|-------|----------|
| Ethereum | 3 blocks |
| Gnosis/Polygon/BNB/Avalanche | 10-20 blocks |
| Arbitrum/Base/Linea/Ink | 20-40 blocks |

Missing the deadline can trigger **immediate denylisting** pending manual inspection.

---

## Layer 3: Governance / Social Consensus (DAO-Enforced, CIP-11+)

These rules are **not automatically enforced** by code. The core team monitors settlements with tooling, flags suspicious behavior, and the DAO votes on slashing via CIPs.

### 3a. Unfair Solutions (CIP-11)

- **Violation**: Providing clearing prices worse than what users would get on reference AMMs (Uniswap, Balancer, Curve, etc.), also called the **EBBO rule** (Ethereum Best Bid/Offer)
- **Consequence**: Monitoring flag, potential slashing at DAO discretion

### 3b. Score Inflation (CIP-11)

- **Violation**: Creating fake tokens or wash-trading to artificially inflate solution scores
- **Consequence**: Slashing

### 3c. Illegal Buffer Usage (CIP-11)

- **Violation**: Using internal buffers beyond legitimate AMM replacement; systematic trading with unsafe tokens; creating buffer attack vectors
- **Consequence**: Flagging, slashing
- **Real-world example**: CIP-22 — Barter Solver failed to revoke approvals on an old contract, a hacker drained ~$166K from the settlement contract. Bond was slashed to reimburse losses.

### 3d. Illegal Surplus Shifts / Local Token Conservation (CIP-11)

- **Violation**: Intentionally transferring surplus between orders that share common tokens (one user's surplus subsidizes another)
- **Consequence**: Slashing

### 3e. Overbidding / Pennying (CIP-13)

- **Violation**: Systematically inflating reported scores beyond `surplus + fees - gas`, expecting rewards to cover losses
- **Detection formula**: `avg(score) > avg(surplus + fees - gas) + epsilon`
- **Consequence**: Slashing (currently retroactive/manual, not automated)

### 3f. Pre/Post Hook Non-Execution (CIP-11)

- **Violation**: Intentionally excluding hooks specified in order app data
- **Consequence**: Slashing

### 3g. Catch-All: Other Malicious Behavior (CIP-11)

- **Violation**: Any intentional harm to users or the protocol not covered above
- **Consequence**: Slashing at DAO discretion
- **Real-world example**: CIP-55 — GlueX solver slashing (passed vote)

---

## Layer 4: Economic Penalties (Reward Mechanism, CIP-38+)

Even without explicit "slashing," the reward formula itself penalizes poor behavior.

### Performance Reward Formula

```
performanceReward_i = cap(totalScore - referenceScore_i - missingScore_i)
```

Where:

- `totalScore` = sum of all winning solutions' scores
- `referenceScore_i` = counterfactual total score if solver i hadn't participated
- `missingScore_i` = scores of solver i's solutions that **reverted** (failed settlements)

Failed settlements directly reduce rewards — and can make them negative.

### Capping (Bounds)

```
cap(x) = max(-c_l, min(c_u, x))
```

| Chain | Lower Bound (max loss per auction) | Upper Bound |
|-------|------------------------------------|-------------|
| Ethereum/Arbitrum/Base | 0.010 ETH | beta x protocol fees earned |
| Gnosis | 10 xDAI | beta x protocol fees earned |
| Polygon | 30 POL | beta x protocol fees earned |
| Avalanche | 0.3 AVAX | beta x protocol fees earned |
| BNB | 0.04 BNB | beta x protocol fees earned |
| Linea/Ink | 0.0015 ETH | beta x protocol fees earned |
| Plasma | 30 XPL | beta x protocol fees earned |

A solver can owe the protocol money if their reverted settlements drag down the total score.

### Buffer Slippage Accounting

Positive/negative slippage from buffer usage is settled **weekly**. Negative slippage = solver pays.

---

## Bonding Requirements (The Stake at Risk)

All DAO-enforced slashing ultimately hits the solver's **bonding pool**.

| Pool Type | Stablecoins | COW Tokens | Notes |
|-----------|-------------|------------|-------|
| **Standard (CoW DAO pool)** | $500,000 | 1,500,000 COW | CoW DAO safe is sole signer |
| **Reduced pool (CIP-44)** | $50,000 to $100,000 over 1 year | 500,000 to 1,000,000 COW over 1 year | Must already be vouched under main pool |

Solvers in the CoW DAO pool also pay a **15% service fee** on weekly COW rewards (starting 6 months after joining).

---

## Summary: What Can Go Wrong for a Solver

| Risk | Enforcement | Automated? | Financial Impact |
|------|------------|------------|------------------|
| Limit price violation | Smart contract | Yes | Tx reverts (gas wasted) |
| Not whitelisted | Smart contract | Yes | Tx reverts |
| Consecutive non-settlement | Autopilot | Yes | Temp ban (5 min) |
| High failure rate | Autopilot | Yes | Temp ban (5 min) |
| Untrusted token internalization | Driver | Yes | Settlement blocked |
| Settlement revert on-chain | Reward formula | Yes | Negative reward (pay protocol) |
| Missed deadline | Autopilot | Yes | Immediate denylist |
| Unfair prices (EBBO) | DAO governance | No | Bond slashing |
| Score inflation | DAO governance | No | Bond slashing |
| Illegal buffer usage | DAO governance | No | Bond slashing |
| Surplus shifting | DAO governance | No | Bond slashing |
| Overbidding | DAO governance | No | Bond slashing |
| Hook non-execution | DAO governance | No | Bond slashing |
| Security negligence | DAO governance | No | Bond slashing (e.g. $166K in CIP-22) |

---

## Sources

- [Solver competition rules — CoW Docs](https://docs.cow.fi/cow-protocol/reference/core/auctions/competition-rules)
- [Solver rewards — CoW Docs](https://docs.cow.fi/cow-protocol/reference/core/auctions/rewards)
- [Bonding pools — CoW Docs](https://docs.cow.fi/cow-protocol/reference/core/auctions/bonding-pools)
- [CIP-11: Rules of the Solver Competition](https://forum.cow.fi/t/cip-11-rules-of-the-solver-competition-status-quo-and-an-update-proposal/1016)
- [CIP-22: Slashing of the Barter Solver](https://forum.cow.fi/t/cip-22-slashing-of-the-barter-solver-responsible-for-a-hack-causing-cow-dao-a-loss-of-1-week-fee-accrual/1440)
- [CIP-38: Solver Computed Fees & Rank by Surplus](https://forum.cow.fi/t/cip-38-solver-computed-fees-rank-by-surplus/2061)
- [CIP-55: Slashing of the GlueX Solver](https://forum.cow.fi/t/cip-55-slashing-of-the-gluex-solver/2649/3)
- [Measuring and banning overbidding — Forum](https://forum.cow.fi/t/measuring-and-banning-overbidding/1874)
