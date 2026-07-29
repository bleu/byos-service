# Contract artifact provenance

Status: accepted

## Context

The contracts live in a separate repo. This service consumes them twice over: `crates/byos-common/abis/*.json` drive the `sol!` bindings that encode `execute` and `settle` calldata, and the e2e harness deploys real bytecode to a local chain. Both were vendored by hand, each recording its source as a commit hash in a comment.

That arrangement drifted in two different ways when byos-contracts landed the floor-and-sweep rework (#27, `31ae5c8`), which changed `execute`'s signature.

The ABIs were regenerated correctly, but `contracts.rs` still named `886ee9c` — a commit from three PRs earlier. Nothing was broken; the record of what we were built against was simply false, so the next person auditing the sync would have checked the wrong tree.

The e2e artifact failed the other way round. `crates/e2e/testdata/artifacts/Escrow.json` matched its recorded pin `ac1e810` exactly, and the pin itself was stale. Because the Escrow constructor deploys the TrampolineFactory, and the factory embeds the Trampoline creation code, that one file shipped a Trampoline with the old four-argument `execute`. The fixture stayed green only because it never called it.

The two failures need different answers. Checking vendored files against their pin catches the first. Only noticing that the pin has fallen behind upstream catches the second.

A third-hand problem: `settlement.rs` cited `c9b317e`, the tip of a contracts feature branch. It was never reachable from contracts `main` and stops resolving entirely once that branch is deleted.

## Decision

- **byos-contracts is a git submodule** at the repo root, pinned to a commit on contracts `main`. The submodule is the single record of which contracts this service targets. No commit hashes in prose, anywhere.
- **The ABIs stay vendored** under `crates/byos-common/abis/`. `cargo build`, `cargo test` and every contributor workflow keep working with no foundry installed and no submodule checked out.
- **`just sync-abis` regenerates them** from the submodule via `forge build` and `jq '.abi'`. The vendored files are generated artifacts and are never hand-edited.
- **CI proves the vendored files match the pin.** A separate `abis` job checks out submodules recursively, installs foundry, runs `just sync-abis`, and fails on a dirty tree. It is separate so the lint and test jobs stay free of both dependencies.
- **Dependabot watches the pin.** The `gitsubmodule` ecosystem opens a PR when contracts `main` moves, so a pin falling behind is an inbox item rather than something discovered later by a failing settlement.
- **E2e deploy artifacts come from the same submodule.** This supersedes [ADR-0009](0009-testing-strategy.md)'s statement that they are regenerated from byos-contracts *releases*, and closes its open question about the regeneration procedure. Releases are a reasonable future refinement; the contracts repo does not cut them today.

Bumping the pin is one deliberate act: move the submodule, run `just sync-abis`, and let the check confirm the bindings followed.

## Alternatives considered

- **Keep hand-maintained hashes, just fix them.** Cheapest. Rejected: it is exactly the arrangement that produced both failures, and correcting four sites without changing the mechanism buys one clean release and no more.
- **Generate bindings at build time from `forge build` output.** One source of truth, no sync step, no possibility of skew. Rejected: it makes foundry a hard dependency of `cargo build` for every contributor and every CI job, to remove a check that costs one job.
- **Point `sol!` at the Solidity sources.** No forge, no vendoring. Rejected: `Escrow` and `TrampolineFactory` import OpenZeppelin, so alloy would need the full remapping set.
- **A bespoke CI job that fetches contracts `main` and warns when the pin is behind.** More control over severity and wording than Dependabot. Rejected as a workflow to maintain for something a standard ecosystem already does.
- **Track byos-contracts releases rather than `main`.** Fewer, more meaningful bumps, and it matches ADR-0009's original language. Rejected for now because the contracts repo does not tag releases pre-audit. Worth revisiting after the audit.

## Consequences

- Working on the ABIs or the e2e fixture needs `git submodule update --init --recursive`. An ordinary build or test run does not, and CI only pays the cost in one job.
- CI gains a foundry dependency in the `abis` job, and that job is the gate on ABI freshness.
- Dependabot will open a PR whenever contracts `main` moves, including for changes that do not touch interfaces. Closing one is cheap; the signal is worth the noise while the contracts churn pre-audit.
- The pin is only as good as the review that bumps it. The sync check proves the bindings match the pin; it says nothing about whether the service still encodes the right calls. Interface changes still need reading.
