# Crate anatomy & internal layering

Status: accepted

> Adopted from [cowprotocol/services](https://github.com/cowprotocol/services), where `crates/driver` and `crates/solvers` are the canonical examples.

## Context

We need a standard shape for crates in this workspace: how binaries are structured, where business logic lives versus IO code, and where wire types go. The services repo has settled answers that its reviewers (the CoW core team is this grant's Reviewer) already know how to read.

## Decision

### Crate set

| Crate | Kind | Role |
|---|---|---|
| `byos` | lib + bin | The BYOS service: public proposal API + solver engine, one process, two listeners ([ADR-0001](0001-proposal-api.md)) |
| `subsolver` | lib + bin | Reference sub-solver: example proposal-API client, e2e-test counterpart, documentation for external teams |
| `proposal-dto` | lib | Serde wire types for the proposal API, shared by server and clients |
| `e2e` | lib + tests | End-to-end orchestration: contracts on anvil + `byos` + `subsolver` ([ADR-0009](0009-testing-strategy.md)) |

New capabilities get their own small, single-purpose, kebab-case-named crates (services has `rate-limit`, `bad-tokens`, `observe`, …) rather than growing a `utils` module.

### Binary crates are lib + bin

`[lib] doctest = false` plus a `[[bin]]`. `main.rs` stays ~10 lines: it calls `crate::start(std::env::args()).await`. Real startup lives in `src/run.rs`: install panic hook → parse args → init tracing → log version and full args → build the app from config → serve with graceful shutdown (SIGINT/SIGTERM). `run()` accepts an optional `oneshot::Sender<SocketAddr>` so tests can bind to port 0 and learn the assigned address — this is what lets the e2e crate start the service in-process.

### domain / infra split

Inside `byos` (and `subsolver` once it grows):

- `domain/` — pure business logic, no IO: proposal store and lifecycle, scoring and selection, eligibility math, attribution. Types here are the CONTEXT.md vocabulary.
- `infra/` — everything touching the outside world: `api/` (axum servers with per-route `dto/` modules), `blockchain/` (RPC, simulation, settlement watcher), `cli.rs`, `config/`, `observe/`, `persistence/` (audit trail).
- DTOs live next to the route or adapter that uses them; conversion to domain types happens at the edge. Wire types shared across crates are extracted to `proposal-dto` (the `solvers-dto` pattern), so the `byos` server and any sub-solver client deserialize one model.

No `boundary/` layer: services uses it as an anti-corruption layer against legacy code it is refactoring away; we have no legacy. If we later vendor code from services, its anyhow-flavored surface gets wrapped at the `infra` edge instead.

### Solver-engine shape

Following `crates/solvers`, the engine is not a trait object: services models its engines as a plain enum with an `async fn solve(&self, auction) -> Vec<Solution>`. BYOS has exactly one engine (the proposal-cache engine), so it is a plain struct with the same signature; the enum indirection can be introduced if a second engine ever exists.

### Wire conventions (proposal-dto and the /solve API)

Match the Solver Engine API conventions from `crates/solvers/openapi.yml`: camelCase JSON (`#[serde(rename_all = "camelCase")]`), 256-bit amounts as decimal strings, addresses and order UIDs as hex strings, RFC3339 deadlines. The `/solve` side of `byos` implements that existing spec as-is (BYOS must look like a vanilla solver engine to the driver); the proposal API defines its own spec in the same idiom, kept as an `openapi.yml` in the `byos` crate.

## Alternatives considered

- **One monolithic crate.** Simpler initially. Rejected — the sub-solver must be independently buildable and vendorable by external teams, and shared wire types must not drag in the whole service.
- **Solver trait with dynamic dispatch.** More extensible in the abstract. Rejected — services itself avoids it; there is exactly one engine, and the enum/struct approach keeps `solve` inlined and simple.
- **Hexagonal/ports-and-adapters naming (`ports/`, `adapters/`).** Same idea, different names. Rejected — `domain`/`infra` is what services reviewers already read fluently.
- **Shared DTOs inside the `byos` crate, re-exported.** Rejected — clients would depend on the full service crate; a `-dto` crate keeps the dependency arrow pointing the right way.

## Consequences

- The e2e crate can drive the real service and the real sub-solver in-process, no docker needed for the common path.
- Per-route `dto/` modules mean some type duplication between DTOs and domain types; that duplication is the point — wire format and domain model evolve independently.
- The `openapi.yml` for the proposal API becomes a deliverable in its own right (the RFP requires a documented public API) and can be linted in CI later, as services does.
