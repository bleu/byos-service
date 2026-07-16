# Observability

Status: accepted

> Adopted from [cowprotocol/services](https://github.com/cowprotocol/services), whose `observe` crate centralizes tracing, metrics, and process hygiene for every service binary.

## Context

BYOS is an operated service with SLOs (see [`docs/metrics-reasoning.md`](../metrics-reasoning.md): `/solve` p99 < 100ms, ingestion p99 < 1s) and an operational runbook deliverable in the RFP. It also has money-moving background jobs (Track A debits) whose actions must be reconstructable after the fact. Observability is therefore a day-one concern, not a hardening pass.

## Decision

Adopt the services observability stack, sized down:

- **tracing + tracing-subscriber** with an EnvFilter driven by the `--log` flag, optional JSON output (`--use-json-logs`) for production log pipelines, and a panic hook that routes panics through tracing so they land in the same pipeline as errors.
- **Per-request spans via tower-http `TraceLayer`** on both listeners, following services' `make_span`/`record_trace_id` pattern so a proposal can be followed from ingestion through simulation to selection by trace id.
- **prometheus `/metrics` and a `/healthz` route on each listener**, like every services binary. First-class metrics follow the SLOs and the money paths: ingestion outcome counters by rejection kind ([ADR-0007](0007-error-handling.md)), `/solve` latency histogram, proposal-cache size, simulation-loop lag, escrow-cache staleness, debit attempts/outcomes, settlement watcher block lag.
- **Graceful shutdown** on SIGINT/SIGTERM (`axum::serve(...).with_graceful_shutdown`), so deploys don't drop in-flight `/solve` calls mid-auction.
- **Never hold a span guard across an await.** Services enforces this via `.clippy.toml` (`await-holding-invalid-types` on `tracing::span::Entered`); we adopt the same clippy config once async code lands.
- **The audit trail is not logging.** The write-behind proposal audit store ([ADR-0001](0001-proposal-api.md)) is dispute evidence with a ≥3-month retention requirement; logs are operational and rotate freely. Neither substitutes for the other.

Deferred until operations demand them: OpenTelemetry OTLP export with cross-service trace propagation (valuable once the CoW driver's `tracing_headers()` are pointed at us in staging), tokio-console, jemalloc heap-dump handlers.

## Alternatives considered

- **Depend on services' `observe` crate directly.** It is `MIT OR Apache-2.0` and importable. Rejected for now — it drags in OTLP/console/jemalloc machinery we're deferring; we re-implement the thin slice we need and can swap to `observe` if drift becomes a maintenance burden.
- **log/env_logger instead of tracing.** Rejected — no spans, no structured fields; correlating a proposal across ingestion, simulation loop, and `/solve` needs span context.
- **Metrics via a hosted APM agent.** Rejected — prometheus is what CoW's infra scrapes; staying native to that keeps the staging integration friction-free.

## Consequences

- Two listeners × (`/metrics`, `/healthz`) plus tracing layers is a little boilerplate per server; it is the same boilerplate services reviewers expect to see.
- Metric names chosen now become dashboard/runbook vocabulary — pick them from CONTEXT.md terms (proposal, ingestion, gatekeeping, debit) and keep them stable.
