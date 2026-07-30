//! Unused crate. Both halves of what it was scoped for have landed elsewhere,
//! so nothing is expected to be added here.
//!
//! The **chain watcher** (poll blocks, detect settlements, attribute them to
//! sub-solvers) is superseded: settlement outcomes arrive from the driver's
//! notifications at `byos::infra::api::notify`, so no block watching is
//! needed (ADR-0010).
//!
//! The **escrow operator** is implemented, in the service:
//! `byos::infra::blockchain::operator` submits the debits and
//! `byos::infra::penalty` drives them (ADR-0003, ADR-0013).
//!
//! Nothing depends on it. It is left in place only because removing a workspace
//! member is a structural decision rather than a documentation fix; deleting it
//! means dropping this directory, the `byos-watcher` line in the root
//! `Cargo.toml`, and its rows in README.md and AGENTS.md.
