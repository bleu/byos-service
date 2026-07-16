# Cargo workspace & tooling

Status: accepted

> Adopted from [cowprotocol/services](https://github.com/cowprotocol/services) (surveyed at `main`, July 2026). BYOS integrates with that codebase's driver, so sharing its build conventions keeps the two repos mutually legible and lets us lift code and reviewers' habits across.

## Context

This repo hosts the BYOS service and its companion crates (reference sub-solver, shared wire types, e2e tests). We need workspace, formatting, linting, and task-runner conventions before any implementation lands. cowprotocol/services is the reference codebase: it is the CoW backend BYOS plugs into, and its conventions are proven on a much larger Rust workspace (40+ crates).

## Decision

Adopt the services conventions, trimmed to this repo's size:

- **Single Cargo workspace**, `resolver = "3"`, members `crates/*`. The root `Cargo.toml` is workspace-only (no root package).
- **All dependencies declared once in `[workspace.dependencies]`** — external and internal alike. Internal crates appear as path deps (`byos = { path = "crates/byos" }`) so any crate can depend on a sibling by bare name with `{ workspace = true }`. Version bumps happen in one place.
- **Workspace lints**: `[workspace.lints]` at the root (starting with `clippy.cast_possible_wrap = "deny"`, extended as needs arise); every crate opts in with `[lints] workspace = true`. No `[workspace.package]` inheritance — each crate states its own version/edition/authors/license, matching services.
- **Edition 2024, stable toolchain** (`rust-toolchain.toml`: stable channel, minimal profile, clippy + rustfmt components).
- **Nightly-only rustfmt**: `rustfmt.toml` copies the services config verbatim (`imports_granularity = "One"`, `group_imports = "StdExternalCrate"`, `format_strings`, comment formatting, etc.). These are unstable options, so formatting runs as `cargo +nightly fmt` — never stable fmt. Format only as the final step of a change.
- **Clippy at `-D warnings`**: `cargo clippy --locked --workspace --all-features --all-targets -- -D warnings`.
- **Justfile as the single command surface** for developers and CI: `fmt`, `fmt-check`, `clippy`, `test-unit`, `test-e2e`, `build`. CI invokes just recipes rather than raw cargo commands so local runs and CI are identical.
- **cargo-nextest as the test runner** (see [ADR-0009](0009-testing-strategy.md)).
- **Licensing**: service crates are `LGPL-3.0-or-later` (matching [`bleu/byos-contracts`](https://github.com/bleu/byos-contracts)); small reusable library crates like `proposal-dto` are `MIT OR Apache-2.0` (matching how services licenses `observe`/`solvers-dto`) so external sub-solver teams can vendor them freely.

Deliberately not adopted (yet):

- **jemalloc/mimalloc allocator switching and tokio-console features** — heap-profiling machinery that earns its keep at services' scale; add if profiling demands it.
- **`.cargo/config.toml` release-debug profile, tombi TOML formatting, trivy/cargo-audit CI jobs** — add cargo-audit when the dependency tree is real; the rest as operational needs appear.
- **A `boundary/` anti-corruption layer** — that exists in services to quarantine legacy code; this repo starts greenfield (see [ADR-0005](0005-crate-anatomy-and-layering.md)).

## Alternatives considered

- **Per-crate dependency versions (no workspace centralization).** Rejected — version skew across crates is the default failure mode of multi-crate repos; services solved it this way and we get it for free.
- **`[workspace.package]` inheritance.** More DRY than repeating version/edition per crate, and arguably the modern default. Rejected in favor of matching services exactly — the repetition cost across four crates is trivial, and consistency with the reference codebase wins.
- **Stable rustfmt with default options.** Avoids the nightly requirement. Rejected — import granularity/grouping is where most formatting churn lives, and diverging from services' style makes cross-repo code movement noisy.
- **Makefile or shell scripts instead of Justfile.** Rejected — services uses just; recipes double as documentation of the exact CI commands.

## Consequences

- Contributors need a nightly toolchain installed for `just fmt`, and `just` itself. Both are one-line installs; the Justfile documents everything else.
- Matching services exactly means inheriting its quirks (no workspace.package, per-crate metadata repetition). Accepted for consistency.
- The lint set starts minimal; tightening it later (more deny-level clippy lints, `.clippy.toml` disallowed-methods) is an additive change.
