# Error handling

Status: accepted

> Adopted from [cowprotocol/services](https://github.com/cowprotocol/services), taking the pattern its own code is migrating *toward* (services still carries anyhow-heavy legacy code it explicitly marks as debt; we start where it is heading).

## Context

Rust offers two idiomatic error styles: typed errors (`thiserror`) that callers can match on, and opaque context-chained errors (`anyhow`) for code whose callers only propagate. Services uses both, with a documented direction: thiserror in new domain/infra code, anyhow quarantined behind its legacy `boundary/` layer.

## Decision

- **thiserror everywhere in `domain/` and `infra/`.** Errors that cross a module boundary are typed enums; validation failures name the rule they broke. The proposal-ingestion pipeline in particular needs typed rejections, because [ADR-0001](0001-proposal-api.md) promises sub-solvers a machine-readable 4xx reason for every rejection.
- **anyhow only at the outermost shell** — `run.rs`/`main` startup, where the only consumer is a log line and a non-zero exit — and in test/setup code, where ergonomics beat taxonomy.
- **HTTP errors follow the services shape** (`driver/src/infra/api/error.rs`): a serializable `Kind` enum (PascalCase on the wire) plus `{ kind, description }` body, converted via a single `From<Kind> for (StatusCode, Json<Error>)`. The proposal API's rejection reasons (bad signature, under-collateralized, simulation revert, rate-limited, fee not covered, …) are one such enum; the `/solve` API reuses the error kinds the driver already expects from solver engines.
- **No panics on request paths.** Panics are reserved for startup invariant violations (bad config caught before serving). A panic hook routes any residual panic through tracing ([ADR-0008](0008-observability.md)).
- **Errors are for callers, logs are for operators**: attach operator context (order UID, signer, proposal id) at the log site via tracing fields, not by stuffing it into error strings.

## Alternatives considered

- **anyhow everywhere.** Fastest to write. Rejected — the ingestion API contractually needs distinguishable rejection reasons, and `downcast_ref` chains are how that goes wrong.
- **thiserror everywhere including binaries' top level.** Rejected — a `StartupError` enum nobody matches on is ceremony; services also treats the shell as anyhow territory.
- **A single repo-wide error enum.** Rejected — it grows unboundedly and couples unrelated modules; per-boundary enums keep each API's failure surface explicit.

## Consequences

- Typed rejection enums double as the proposal API's documented error catalogue — the OpenAPI spec and the code share one source of truth.
- More upfront typing than anyhow-style development; the payoff is that sub-solver-facing behavior (which 4xx, which reason) is compiler-checked rather than string-matched.
