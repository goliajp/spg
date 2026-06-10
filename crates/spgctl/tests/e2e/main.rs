//! v7.20 test-speed A — spgctl (perf_gate stays standalone: timing-sensitive) integration tests
//! merged into one target: one link instead of one per file,
//! libtest parallelises modules in-process.

#[allow(clippy::uninlined_format_args)]
mod e2e_revert;
#[allow(clippy::uninlined_format_args)]
mod e2e_wal_lint;
mod pitr_e2e;
