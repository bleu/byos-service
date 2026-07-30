# Single command surface for devs and CI (mirrors cowprotocol/services).

# Format all crates. Requires nightly rustfmt (unstable options in rustfmt.toml).
fmt:
    cargo +nightly fmt --all

fmt-check:
    cargo +nightly fmt --all -- --check

clippy:
    cargo clippy --locked --workspace --all-features --all-targets -- -D warnings

# Validate and lint crates/byos/openapi.yml, mirroring the `openapi` job in
# cowprotocol/services: swagger-cli for structural validity, spectral for
# style, ruleset in .spectral.yaml. No code, test or handler reads that spec,
# so this is the only thing keeping it honest — ADR-0005's "linted in CI
# later, as services does".
#
# Needs node; nothing else in this file does. Warnings do not fail the run —
# spectral only exits non-zero on `error`, and services keeps the same posture
# (its own orderbook spec carries 30 warnings today).
#
# swagger-cli is abandoned upstream and prints a deprecation notice; it is what
# services runs, and @redocly/cli is the successor whenever we want to move.
#
# Both majors are pinned. A spectral major adds rules to `spectral:oas` and can
# change what exits non-zero, so an unpinned version turns CI red on a PR that
# never touched the spec — and `npx --yes` on a floating version is the wider
# supply-chain surface too.
lint-openapi:
    npx --yes @apidevtools/swagger-cli@4 validate crates/byos/openapi.yml
    npx --yes @stoplight/spectral-cli@6 lint crates/byos/openapi.yml

# Unit tests. Drop --no-tests=pass once the first test lands.
test-unit:
    cargo nextest run --no-tests=pass

# DB-backed service-level tests (proposal API + audit trail). Needs the
# compose Postgres: `docker compose up -d postgres`.
test-db:
    cargo nextest run -p byos --run-ignored ignored-only

# Drop every leftover per-test database. The harness sweeps ones older than a
# few hours on its own, so this is for reclaiming space now — after a heavy
# session, or when Postgres starts refusing connections with
# "No space left on device".
test-db-clean:
    #!/usr/bin/env bash
    set -euo pipefail
    # One psql invocation, not one per database: `docker compose exec` reads
    # stdin, so calling it inside a `while read` loop swallows the list and
    # drops exactly one.
    names=$(docker compose exec -T postgres psql -U postgres -tAc \
        "SELECT datname FROM pg_database WHERE datname LIKE 'byos_test_%'" | sed '/^$/d')
    if [ -z "$names" ]; then echo "no leftover test databases"; exit 0; fi
    printf 'dropping %s test database(s)\n' "$(printf '%s\n' "$names" | wc -l | tr -d ' ')"
    printf '%s\n' "$names" \
        | sed 's/.*/DROP DATABASE IF EXISTS "&";/' \
        | docker compose exec -T postgres psql -U postgres -q
    printf '%s remaining\n' "$(docker compose exec -T postgres psql -U postgres -tAc \
        "SELECT count(*) FROM pg_database WHERE datname LIKE 'byos_test_%'" | tr -d ' ')"

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

# ---------------------------------------------------------------------------
# Running byos on the host by hand
# ---------------------------------------------------------------------------
# Shared by `byos-local` and `propose`, because two copies of the domain
# constants drift and a drifted domain fails in a way that looks like the
# service's fault: the POST is accepted and the read then 404s. The factory
# is the visibly-fake one from the sub-solver tests; the key is anvil
# account 4, matching COW-1236's account map.
local-chain-id := "31337"
local-factory := "0x00000000000000000000000000000000000fac70"
local-subsolver-key := "0x47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a"
local-byos-url := "http://127.0.0.1:9585"
local-database := "byos_dev"

# Run byos against the compose Postgres with no chain (AcceptAll validation).
# Listeners take their defaults: 127.0.0.1:9585 public, 127.0.0.1:9586 internal.
byos-local:
    #!/usr/bin/env bash
    set -euo pipefail
    # `--wait` blocks on the compose healthcheck. Plain `up -d` returns once
    # the container has started, which on a cold container is well before
    # Postgres accepts connections — the psql below then dies on "the database
    # system is starting up".
    docker compose up -d --wait postgres
    # `connect_and_migrate` migrates but does not create, so the database has
    # to exist before the service boots, and Postgres has no CREATE DATABASE
    # IF NOT EXISTS.
    #
    # Deliberately not `psql ... | grep -q 1 || createdb`: that pipeline reads
    # a psql that could not answer as "no database" and tries to create one
    # that is already there. Capturing first keeps the two apart — `set -e`
    # aborts on a psql that failed, and only a genuine empty result creates.
    exists=$(docker compose exec -T postgres psql -U postgres -tAc \
        "SELECT 1 FROM pg_database WHERE datname = '{{local-database}}'")
    if [ -z "$exists" ]; then
        docker compose exec -T postgres psql -U postgres -q \
            -c 'CREATE DATABASE {{local-database}}'
    fi
    # 2s rather than the 12s default: right for production, too slow for a
    # tool you re-run while debugging.
    cargo run -p byos -- \
        --chain-id {{local-chain-id}} \
        --trampoline-factory {{local-factory}} \
        --database-url postgres://postgres:postgres@localhost:5432/{{local-database}} \
        --validation-interval-secs 2

# Submit one signed proposal to `just byos-local` and follow its status.
# `@`, so the recipe echo does not put the signing key on the terminal. It is
# a published anvil key, but the habit is worth keeping.
propose:
    @SUBSOLVER_PRIVATE_KEY={{local-subsolver-key}} \
        cargo run -q -p subsolver --example propose -- \
        --byos-url {{local-byos-url}} \
        --chain-id {{local-chain-id}} \
        --trampoline-factory {{local-factory}}

# Regenerate the vendored contract artifacts from the pinned byos-contracts
# submodule (ADR-0014): ABI-only files for the service bindings, plus the
# e2e harness's Escrow artifact, which also carries creation bytecode because
# the harness deploys it. Needs foundry and jq; nothing else in this file does,
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
    # The e2e fixture deploys the Escrow, so it needs creation bytecode too.
    # Only the Escrow: its constructor deploys the TrampolineFactory, which in
    # turn embeds the Trampoline creation code.
    jq '{abi, bytecode}' byos-contracts/out/Escrow.sol/Escrow.json \
        > crates/e2e/testdata/artifacts/Escrow.json
