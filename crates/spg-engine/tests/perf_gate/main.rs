// Timing-sensitive perf/SLO target — meaningless under debug
// codegen, so it only compiles in release (the perf_gate
// convention; budgets in BUDGETS.md).
#![cfg(not(debug_assertions))]
// Test-gate allow-list — see crates/spg-crypto/tests/perf_gate.rs.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::useless_conversion,
    clippy::similar_names,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]

//! spg-engine perf gate — every former standalone `perf_*` binary
//! merged into ONE release-only target (test-speed A pattern).
//!
//! fast tier: every non-ignored test — hard budget / ratio gates,
//! run by `scripts/gate.sh gates`.
//! full tier: `#[ignore]`d exploratory sweeps — run with
//! `--include-ignored` via `scripts/gate.sh gates --full`.
//!
//! Each timed test takes `perf_lock()` so in-binary parallelism
//! can't skew the numbers (same pattern as spg-storage perf_gate).

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::Duration;

fn perf_lock() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = L
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    thread::sleep(Duration::from_millis(500));
    guard
}

mod join_reorder;
mod plan_cache;
mod select_where;
mod stages_knn;
