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

# Unit tests.
test-unit:
    cargo nextest run

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

# Bring up the compose Postgres and make sure `name` exists.
# `connect_and_migrate` migrates but does not create, so the database has to
# exist before the service boots, and Postgres has no CREATE DATABASE IF NOT
# EXISTS.
[private]
_ensure-db name:
    #!/usr/bin/env bash
    set -euo pipefail
    # `--wait` blocks on the compose healthcheck. Plain `up -d` returns once
    # the container has started, which on a cold container is well before
    # Postgres accepts connections — the psql below then dies on "the database
    # system is starting up".
    docker compose up -d --wait postgres
    # Deliberately not `psql ... | grep -q 1 || createdb`: that pipeline reads
    # a psql that could not answer as "no database" and tries to create one
    # that is already there. Capturing first keeps the two apart — `set -e`
    # aborts on a psql that failed, and only a genuine empty result creates.
    exists=$(docker compose exec -T postgres psql -U postgres -tAc \
        "SELECT 1 FROM pg_database WHERE datname = '{{name}}'")
    if [ -z "$exists" ]; then
        docker compose exec -T postgres psql -U postgres -q \
            -c 'CREATE DATABASE {{name}}'
    fi

# Run byos against the compose Postgres with no chain (AcceptAll validation).
# Listeners take their defaults: 127.0.0.1:9585 public, 127.0.0.1:9586 internal.
byos-local: (_ensure-db local-database)
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

# ---------------------------------------------------------------------------
# The offline-mode demo stack: BYOS as the only solver in the auction
# ---------------------------------------------------------------------------
# COW-1236. A full CoW stack (real orderbook, autopilot, driver, baseline) from
# the pinned offline-mode submodule, with `byos` and `subsolver` running on the
# host. See README.md for the walkthrough; docs/adr/0009-testing-strategy.md
# for why the overlay lives here rather than in the submodule.
#
# The account map, from anvil's standard test mnemonic. Named here rather than
# described in prose so the recipes, driver.toml and compose.byos.yml cannot
# drift apart — offline-mode's own fixture works the same way, with baseline's
# key sitting in its driver.toml.
anvil-baseline := "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"      # account 0
anvil-escrow-operator := "0x70997970C51812dc3A010C7d01b50e0d17dc79C8" # account 1
anvil-escrow-admin := "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"  # account 2
anvil-byos-solver := "0x90F79bf6EB2c4f870365E785982E1f101E93b906"   # account 3
anvil-sub-solver := "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"    # account 4
anvil-trader := "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"        # account 5

anvil-deployer-key := "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
anvil-operator-key := "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
anvil-trader-key := "0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba"

# Chain constants. `stack-chain-id` is 1, not `local-chain-id`'s 31337: the
# fixture's anvil runs `--chain-id 1` so the mainnet contract addresses in the
# committed state are self-consistent. Getting this wrong does not error —
# the EIP-712 domain differs, so `POST /proposals` recovers a different
# proposer, 202s, and the read then 404s. Same trap for the factory, which is a
# real deployment here rather than `local-factory`'s visibly-fake address.
stack-chain-id := "1"
stack-rpc-url := "http://127.0.0.1:8545"
stack-orderbook-url := "http://127.0.0.1:8080"
stack-database := "byos_stack"

# GPv2, at their real mainnet addresses in the committed state.
stack-settlement := "0x9008D19f58AAbD9eD0D60971565AA8510560ab41"
stack-authenticator := "0x2c4c28DDBdAc9C5E7055b4C863b72eA0149D8aFE"

# The BYOS contracts, CREATE2 with a fixed salt from the vendored Escrow
# artifact, so they are the same on every run. `stack-up` deploys them and
# fails loudly if what lands does not match these — regenerating the artifact
# (`just sync-abis`) shifts both addresses.
stack-escrow := "0x8DE7F64B42635Ff2Ee83F6413936E6F02cBeb520"
stack-trampoline-factory := "0xDA50c08F941FEFb955e0a0B62160da9A809c0faC"

# Collateral. 1 ETH deposited against a 0.01 ETH floor: the real bar is
# `gas * gas_price + min_collateral`, and being far above it keeps a proposal
# from being rejected for a reason the demo is not about.
stack-collateral-wei := "1000000000000000000"
stack-min-collateral := "10000000000000000"
stack-gas-price := "1000000000"

# Only what the demo needs. Skips adminer, watch-tower, grafana, prometheus and
# the two UIs — the UIs build from `modules/frontend`, the cowswap monorepo,
# which is the nested submodule worth not cloning.
#
# tempo is skipped for a different reason: it is pinned to `grafana/tempo:latest`
# and that image has drifted past the committed tempo.yaml, so it crashloops on
# "field ingester not found". The services point `TRACING_COLLECTOR_ENDPOINT` at
# it and come up healthy regardless — traces go nowhere, which costs this demo
# nothing.
stack-services := "chain-deployer chain db db-migrations coingecko-mock orderbook autopilot driver baseline"
stack-compose := "docker compose -f offline-mode/docker-compose.yml -f dev/offline-mode/compose.byos.yml"

# Exported for every recipe, because compose needs them to parse the overlay at
# all — `stack-down` and `stack-logs` too, not just `stack-up`.
export BYOS_OVERLAY_DIR := justfile_directory() / "dev/offline-mode"
# Two host-port collisions with our own compose and with byos itself. Both are
# publish-only remaps: inside the stack, services still reach `db:5432` and
# scrape the orderbook on 9586.
#   5432 — offline-mode's Postgres against ours.
#   9586 — offline-mode publishes the orderbook's metrics port, which is byos's
#          internal listener. The driver has to reach byos there, so the
#          orderbook's publish moves instead.
export PORT_DB := "5433"
export PORT_ORDERBOOK_METRICS := "9587"
# offline-mode pins a foundry nightly, so a matching local install prints a
# five-line "this is a nightly build" banner on every cast invocation. stack-up
# makes six of them.
export FOUNDRY_DISABLE_NIGHTLY_WARNING := "1"

# Boot the stack and re-apply every chain cheat BYOS needs.
#
# Idempotent and meant to be re-run: anvil's cheats (the solver whitelist, the
# deployed Escrow, the collateral) live in memory and are lost on every stack
# restart. Nothing here is baked into offline-mode's committed state — COW-1237
# covers that.
#
# Needs the service images built once; `{{stack-compose}} build` is 20-60
# minutes cold. Needs foundry (cast) and jq.
[doc('Boot the offline-mode stack with BYOS as the only solver (idempotent)')]
stack-up:
    #!/usr/bin/env bash
    set -euo pipefail
    # offline-mode's .env is gitignored, so a fresh clone has none. It feeds
    # both compose interpolation and the deploy scripts' address map.
    if [ ! -f offline-mode/.env ]; then
        cp offline-mode/.env.example offline-mode/.env
    fi
    if [ ! -f offline-mode/modules/services/Cargo.toml ]; then
        echo "offline-mode/modules/services is empty — the driver, orderbook," >&2
        echo "autopilot and baseline images all build from it. Run:" >&2
        echo "  git -C offline-mode submodule update --init modules/services" >&2
        exit 1
    fi
    {{stack-compose}} up -d --wait {{stack-services}}

    echo "==> whitelisting BYOS's account as a solver"
    if [ "$(cast call {{stack-authenticator}} 'isSolver(address)(bool)' \
            {{anvil-byos-solver}} --rpc-url {{stack-rpc-url}})" != "true" ]; then
        # A real addSolver call rather than offline-mode's direct poke at the
        # solvers mapping, which hardcodes that the mapping is at slot 1. The
        # manager is a mainnet EOA baked into the state: no key we hold, and no
        # balance, so fund it before impersonating it.
        manager=$(cast call {{stack-authenticator}} 'manager()(address)' --rpc-url {{stack-rpc-url}})
        cast rpc anvil_setBalance "$manager" 0xde0b6b3a7640000 --rpc-url {{stack-rpc-url}} > /dev/null
        cast rpc anvil_impersonateAccount "$manager" --rpc-url {{stack-rpc-url}} > /dev/null
        cast send {{stack-authenticator}} 'addSolver(address)' {{anvil-byos-solver}} \
            --from "$manager" --unlocked --rpc-url {{stack-rpc-url}} > /dev/null
        cast rpc anvil_stopImpersonatingAccount "$manager" --rpc-url {{stack-rpc-url}} > /dev/null
    fi

    echo "==> deploying the Escrow"
    # Prints `escrow=` and `trampoline_factory=` for the eval. The submitter is
    # BYOS's account, so it is the only one that can settle through the Escrow.
    eval "$(DEPLOYER_PRIVATE_KEY={{anvil-deployer-key}} \
        cargo run -q -p e2e --example deploy-escrow -- \
        --rpc-url {{stack-rpc-url}} \
        --admin {{anvil-escrow-admin}} \
        --operator {{anvil-escrow-operator}} \
        --submitter {{anvil-byos-solver}})"
    if [ "$escrow" != "{{stack-escrow}}" ] || \
       [ "$trampoline_factory" != "{{stack-trampoline-factory}}" ]; then
        echo "the BYOS contracts landed at addresses this Justfile does not expect:" >&2
        echo "  escrow             $escrow (expected {{stack-escrow}})" >&2
        echo "  trampoline factory $trampoline_factory (expected {{stack-trampoline-factory}})" >&2
        echo "The vendored Escrow artifact changed. Update stack-escrow and" >&2
        echo "stack-trampoline-factory, and dev/offline-mode/subsolver.toml." >&2
        exit 1
    fi

    echo "==> funding the sub-solver and depositing collateral"
    cast rpc anvil_setBalance {{anvil-sub-solver}} 0x21e19e0c9bab2400000 \
        --rpc-url {{stack-rpc-url}} > /dev/null
    # `cast call` appends a human-readable form ("1000000000000000000 [1e18]"),
    # hence the first field. Both sides fit in the 64-bit signed ints bash
    # compares with; a target above ~9.2 ETH would not.
    balance=$(cast call {{stack-escrow}} 'effectiveBalance(address)(uint256)' \
        {{anvil-sub-solver}} --rpc-url {{stack-rpc-url}} | awk '{print $1}')
    if [ "$balance" -lt {{stack-collateral-wei}} ]; then
        cast send {{stack-escrow}} 'deposit(address)' {{anvil-sub-solver}} \
            --value {{stack-collateral-wei}} --private-key {{local-subsolver-key}} \
            --rpc-url {{stack-rpc-url}} > /dev/null
    fi

    echo
    echo "stack up. BYOS is the only solver in the auction."
    echo "  orderbook  {{stack-orderbook-url}}"
    echo "  chain      {{stack-rpc-url}}"
    echo "  escrow     {{stack-escrow}}"
    echo "next: just byos-stack, then just subsolver-stack, then just stack-order"

[doc('Tear the offline-mode stack down')]
stack-down:
    {{stack-compose}} down

[doc('Follow the stack containers that decide who wins an auction')]
stack-logs *services="autopilot driver":
    {{stack-compose}} logs -f {{services}}

# Run byos against the stack. Same shape as `byos-local` plus the chain-aware
# flags; the domain constants differ (see stack-chain-id) so they cannot be
# shared with it.
[doc('Run byos on the host against the offline-mode stack')]
byos-stack: (_ensure-db stack-database)
    #!/usr/bin/env bash
    set -euo pipefail
    # --internal-addr 0.0.0.0:9586 because the driver is in a container and
    # `/solve` has to be reachable from it. The default is loopback for a
    # reason (the proposal book /solve returns is MEV-relevant); widening it is
    # a local-dev choice, not a deployment pattern. See the README.
    #
    # OPERATOR_PRIVATE_KEY via the environment rather than
    # --operator-private-key: CLI arguments are visible to other users via ps.
    OPERATOR_PRIVATE_KEY={{anvil-operator-key}} \
    cargo run -p byos -- \
        --chain-id {{stack-chain-id}} \
        --trampoline-factory {{stack-trampoline-factory}} \
        --database-url postgres://postgres:postgres@localhost:5432/{{stack-database}} \
        --internal-addr 0.0.0.0:9586 \
        --rpc-url {{stack-rpc-url}} \
        --orderbook-url {{stack-orderbook-url}} \
        --escrow-address {{stack-escrow}} \
        --settlement-address {{stack-settlement}} \
        --min-collateral {{stack-min-collateral}} \
        --default-gas-price {{stack-gas-price}} \
        --validation-interval-secs 2

# Run the reference sub-solver against the stack: it discovers orders from the
# real orderbook, routes them through the local Uniswap V2, and submits signed
# proposals to `just byos-stack`. `@`, so the recipe echo does not put the
# signing key on the terminal.
[doc('Run the reference sub-solver against the offline-mode stack')]
subsolver-stack:
    @SUBSOLVER_PRIVATE_KEY={{local-subsolver-key}} \
        cargo run -p subsolver -- \
        --config dev/offline-mode/subsolver.toml \
        --orderbook-url {{stack-orderbook-url}} \
        --byos-url {{local-byos-url}} \
        --rpc-url {{stack-rpc-url}}

# Place one order on the stack's orderbook, as the trader (anvil account 5).
# The 3% surplus default is offline-mode's, and is roughly the margin baseline
# needs before it will engage at all.
[doc('Place one order on the orderbook, as the trader')]
stack-order sell="GNO" buy="WETH" amount="10e18" surplus="3":
    #!/usr/bin/env bash
    set -euo pipefail
    cd offline-mode
    # node_modules is gitignored, and the containerised deployer installs into
    # the container, not here.
    if [ ! -d node_modules ]; then
        npm install --legacy-peer-deps
    fi
    # just does not echo shebang recipe bodies, so the trader key below stays
    # off the terminal without the `@` that `propose` needs.
    npm run --silent order:playground -- \
        --sellToken {{sell}} --buyToken {{buy}} --sellAmount {{amount}} \
        --surplus {{surplus}} --from {{anvil-trader-key}}

# Did BYOS settle? Reads the newest GPv2 Settlement event off the chain and
# compares its solver to BYOS's account.
#
# The chain is the referee here, not our own records: `/notify` tells byos what
# the driver believes happened, and the point of the demo is that an
# independent observer can see BYOS's address on the settlement.
[doc('Read the newest settlement off the chain and check its solver is BYOS')]
stack-settled:
    #!/usr/bin/env bash
    set -euo pipefail
    logs=$(cast logs --address {{stack-settlement}} 'Settlement(address)' \
        --from-block 0 --json --rpc-url {{stack-rpc-url}})
    total=$(jq 'length' <<< "$logs")
    if [ "$total" -eq 0 ]; then
        echo "no settlement on the chain yet" >&2
        exit 1
    fi
    # The solver is the event's only (indexed) argument, so it is topic 1.
    ours=$(jq -r --arg s "$(echo {{anvil-byos-solver}} | tr 'A-Z' 'a-z')" \
        '[.[] | select((.topics[1] | "0x" + .[26:]) == $s)] | length' <<< "$logs")
    latest=$(jq -r '.[-1]' <<< "$logs")
    solver=$(cast to-check-sum-address "0x$(jq -r '.topics[1]' <<< "$latest" | cut -c27-)")
    echo "settlements: $total total, $ours by BYOS"
    # blockNumber comes back hex from the node, hence to-dec.
    echo "latest:      block $(cast to-dec "$(jq -r '.blockNumber' <<< "$latest")") tx $(jq -r '.transactionHash' <<< "$latest")"
    echo "solver:      $solver"
    if [ "$solver" = "{{anvil-byos-solver}}" ]; then
        echo "match: that is BYOS's account (anvil 3)."
    else
        echo "mismatch: expected BYOS's account {{anvil-byos-solver}} (anvil 3)." >&2
        if [ "$solver" = "{{anvil-baseline}}" ]; then
            echo "That is baseline (anvil 0) — it is still in the autopilot's DRIVERS." >&2
        fi
        exit 1
    fi

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
