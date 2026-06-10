// Timing-sensitive perf/SLO target — meaningless under debug
// codegen, so it only compiles in release (the perf_gate
// convention). Debug `cargo test` sees an empty binary; the
// real gate is `cargo test --release --test perf_gate`.
#![cfg(not(debug_assertions))]

//! spg-server perf gate — the former standalone `perf_*` test
//! binaries merged into ONE release-only target (test-speed A
//! pattern, mirroring `tests/e2e/main.rs`).
//!
//! fast tier (non-ignored, run by `scripts/gate.sh gates`):
//!   - `one_b_rows::pipeline_sanity_50k_rows` — ingest→freeze→
//!     compact→restart pipeline sanity at 50K rows
//!   - `parallel_freezer::four_worker_prepare_speedup_scales` —
//!     v6.7.4 ship-gate (host-tiered floor)
//!
//! full tier (`#[ignore]`, run via `scripts/gate.sh gates --full`
//! → `--include-ignored`):
//!   - `one_b_rows::cold_start_under_120s` — L2 gate at
//!     `SPG_PERF_1B_ROW_BUDGET` rows (default 1M)
//!   - `prefetch::four_worker_pool_speedup_at_least_1_3x` —
//!     v6.7.6 ship-gate; I/O-topology-sensitive, only meaningful
//!     on local-NVMe hosts (cloud-runner disks invert under
//!     4-way reads)
//!   - `sq8::*` — 1M dim-128 kNN p50 + RSS gates (minutes)
//!   - `concurrency::concurrency_bench` — exploratory throughput
//!   - `prepared_vs_simple::*` — exploratory wire-path micro-bench
//!
//! Every timed test takes `perf_lock()` so in-binary parallelism
//! can't skew the numbers (same pattern as spg-storage perf_gate).

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::Duration;

#[path = "../common/mod.rs"]
mod common;

fn perf_lock() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = L
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    thread::sleep(Duration::from_millis(500));
    guard
}

#[allow(clippy::uninlined_format_args, unsafe_code)]
mod concurrency;
#[allow(clippy::uninlined_format_args, unsafe_code)]
mod one_b_rows;
mod parallel_freezer;
#[allow(
    clippy::uninlined_format_args,
    clippy::used_underscore_binding,
    unsafe_code
)]
mod prefetch;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_variables
)]
mod prepared_vs_simple;
mod sq8;
