# Single command surface for devs and CI (mirrors cowprotocol/services).

# Format all crates. Requires nightly rustfmt (unstable options in rustfmt.toml).
fmt:
    cargo +nightly fmt --all

fmt-check:
    cargo +nightly fmt --all -- --check

clippy:
    cargo clippy --locked --workspace --all-features --all-targets -- -D warnings

# Unit tests. Drop --no-tests=pass once the first test lands.
test-unit:
    cargo nextest run --no-tests=pass

# DB-backed service-level tests (audit trail). Needs the compose Postgres:
# `docker compose up -d postgres`.
test-db:
    cargo nextest run -p byos --run-ignored ignored-only -E 'test(audit_db)'

# E2e tier 1: byos + reference subsolver in-process against plain anvil
# (preloaded state file). Ignored by default; single-threaded (shared chain state).
test-e2e:
    cargo nextest run -p e2e --test-threads 1 --run-ignored ignored-only -E 'not test(full_stack)'

# E2e tier 2: full CoW stack via offline-mode (real autopilot + driver + baseline).
# Assumes the offline-mode stack is up with the BYOS overlay applied. See ADR-0009.
test-e2e-full:
    cargo nextest run -p e2e full_stack --test-threads 1 --run-ignored ignored-only

build:
    cargo build --workspace
