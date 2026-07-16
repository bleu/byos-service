# BYOS Metrics Reasoning

Documentation of the reasoning behind each metric target for the BYOS project.

## North Star

**December BYOS revenue covers infra + maintenance raw costs (~$500/month)**

### Cost breakdown

| Item | Monthly cost | Reasoning |
|------|-------------|-----------|
| Hosting | $100 | One 8 vCPU + 16 GB machine (based on prior infra experience running 3 similar machines at $100/month each) |
| RPC | $150 | ~10 requests/second via DRPC |
| Dev maintenance | $250 | ~4 hours/week at internal dev cost |
| **Total** | **$500** |  |

### Revenue estimation

**Data sources:** CoW Protocol solver competition dashboard (data from last 30 days, accessed July 2026).

**Market context:**
- CoW Protocol executes ~10,000 trades/day across 25 active solvers
- All-time volume: \$206B all-time surplus: $1.24B (~0.6% surplus/volume ratio)
- COW token price at time of analysis: $0.15

**Reward per trade for comparable solvers** (weekly COW rewards / weekly trade count):

| Solver | Trades/day | COW/trade | USD/trade | Notes |
|--------|-----------|-----------|-----------|-------|
| Rizzolver | 358 | 177 | $26.55 | Top performer, high volume |
| Tsolver | 143 | 80 | $12.01 | |
| Kipseli | 380 | 24.5 | $3.68 | 15% service fee applies |
| Sector | 238 | 14.8 | $2.22 | 15% service fee applies |
| NativeFi | 33 | 13.7 | $2.06 | Small solver, good comparable |
| Quasi | 309 | 10.3 | $1.54 | |
| TrustedVolumes | 34 | 9.7 | $1.46 | Small solver, good comparable |
| Horadrim | 101 | 9.0 | $1.35 | |
| Fractal | 234 | 8.8 | $1.33 | |
| Helixbox | 88 | 1.5 | $0.22 | |

**Breakeven calculation:**

- Target: $500/month after 15% service fee
- Gross needed: $500 / 0.85 = ~\$588/month
- At $0.15/COW: ~3,922 COW/month → ~912 COW/week
- At ~10 COW/trade (mid-tier solver level): **~100 trades/week (~14/day)**

This is conservative relative to the market — 14 trades/day represents ~0.14% of CoW's daily volume, and well below what even small active solvers achieve.

---

## Engineering Metrics

### Hours dedicated to audit remediation fixes < 40

40 hours corresponds to one full dev-week. The M3 milestone allocates 1 week for audit remediation (step 8 in the proposal). Staying under 40 hours is a proxy for contract quality coming out of M1 — if the audit surfaces issues requiring significantly more than a week of fixes, it signals the test suite and offline-mode test round didn't catch enough.

### BYOS API responses p99

**`GET /proposals/{subsolver_address}` p99 < 50ms**

This endpoint lists a sub-solver's active/recent proposals. Served from an in-memory cache filtered by address. Even with hundreds of active proposals, this is a straightforward cache lookup.

**`GET /solve` p99 < 100ms**

The hot path called by the CoW driver during auctions. The driver gives solvers a 15-second deadline (configurable via `solve_deadline` in the autopilot), but BYOS serves entirely from an in-memory cache — no simulation, no RPC, no DB queries. It picks the highest-scoring proposal per order UID and returns it. 100ms is conservative relative to the 15s deadline but ensures BYOS is never a bottleneck in the auction cycle.

### Proposal ingestion time p99 < 1s

This is the async pipeline after `POST /proposals` returns the proposal ID to the sub-solver. It includes:

| Step | Estimated latency |
|------|------------------|
| EIP-712 signature recovery + validation | ~10-20ms |
| Escrow balance check (cached or RPC) | ~50-100ms |
| Interactions hash verification | ~5ms |
| Simulation (RPC call via DRPC) | ~500ms |
| Scoring (surplus + fee - gas) + cache insert | ~10ms |
| **Total expected** | **~600-650ms** |

The simulation RPC call is the main bottleneck (~500ms on DRPC). The 1s target gives ~50% headroom over the expected ~650ms for slow RPC responses, retries, or GC pauses.
