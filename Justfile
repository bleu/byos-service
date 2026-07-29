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

# DB-backed service-level tests (proposal API + audit trail). Needs the
# compose Postgres: `docker compose up -d postgres`.
test-db:
    cargo nextest run -p byos --run-ignored ignored-only

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

# Regenerate the vendored contract ABIs from the pinned byos-contracts
# submodule (ADR-0014). Needs foundry and jq; nothing else in this file does,
# and `just build` never runs it. CI runs it and fails on a dirty tree.
sync-abis:
    #!/usr/bin/env bash
    set -euo pipefail
    # Populate the submodule when it is empty (fresh clone, or a new worktree —
    # worktrees do not inherit submodule contents). Never run it otherwise:
    # `git submodule update` checks out the commit recorded in the index, so on
    # an in-progress pin bump it would silently rewind the submodule and
    # regenerate the ABIs from the old contracts, leaving a clean diff.
    if [ ! -e byos-contracts/foundry.toml ]; then
        git submodule update --init --recursive byos-contracts
    fi
    (cd byos-contracts && forge build -q)
    for contract in Trampoline TrampolineFactory Escrow; do
        jq '.abi' "byos-contracts/out/$contract.sol/$contract.json" \
            > "crates/byos-common/abis/$contract.json"
    done
