//! Placeholder crate — no code lives here yet, and both halves of what it was
//! scoped for have since landed elsewhere.
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
//! Nothing depends on this crate. It is left in place only because removing a
//! workspace member is a structural decision rather than a documentation fix —
//! if no use emerges, delete it along with its README, AGENTS.md, and
//! byos-common mentions.
