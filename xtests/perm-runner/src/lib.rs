//! spg-perm-runner — v7.38 element B library surface.
//!
//! This crate is primarily a binary (`spg-perm-runner`), but the
//! v7.38 differential oracle (`spg-oracle-runner`) calls into it
//! in-process for the "fast-tier self-diff" path documented in
//! `xtests/oracle/src/self_diff.rs`. The lib surface exposes the
//! permutation discovery + execution functions; the bin layer
//! still owns CLI parsing, the toml schema, fork orchestration,
//! and aggregate reporting.
//!
//! Public re-exports below mirror what the bin's `main.rs` calls
//! into — anything callers need stays here behind `pub`.

pub mod permutation;
pub mod report;
pub mod runner;

pub use permutation::{Mode, PermFile, Permutation};
pub use report::{format_perm_report, write_perm_report};
pub use runner::{
    FixtureResult, PermReport, PermStatus, discover_corpus, discover_sample, run_permutation,
};
