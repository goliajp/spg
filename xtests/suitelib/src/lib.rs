//! v7.38 suite library — the pieces `suite-run` orchestrates with.
//!
//! Design is fixed in `.claude/testsuite/AUDIT-2026-08-17.md` (D1-D24);
//! this crate implements, it does not decide. Layers, dependencies
//! pointing inward only:
//!
//! - [`proclib`]   — start/stop/kill real server processes, ports from
//!   the suite's own range, exit-time reaping with a printed roster.
//! - [`reportlib`] — per-step timing ledger, JSON reports keyed by
//!   run-id, structure-only diffs between runs (durations are info,
//!   ±20% out-of-band warnings — audit A14).
//! - [`normlib`]   — output normalization rules as data (MTR
//!   `replace_*` idea). Placeholder until S2.5.
//! - [`snapdiff`]  — before/after catalog and /tmp snapshots per
//!   corpus file (MTR check-testcase idea). Placeholder until S2.4.
//!
//! No third-party dependencies on purpose: this crate sits on every
//! precommit run's critical path, and its own compile time is part of
//! the tier budget (audit A13).

pub mod config;
pub mod crategraph;
pub mod proclib;
pub mod reportlib;
pub mod steps;
pub mod wireclient;

pub mod isolib;
pub mod normlib;

pub mod snapdiff {
    //! The check-testcase half landed in the slt runner itself
    //! (`Runner::leftover_objects` plus the per-file leak warning in
    //! its main.rs) because the check needs the engine in hand — the
    //! corpus harness is its natural home. The tmp-snapshot half
    //! arrives with the full tier. This module stays as the layer-map
    //! anchor.
}
