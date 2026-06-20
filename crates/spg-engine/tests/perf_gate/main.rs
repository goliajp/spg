// Timing-sensitive perf/SLO target — meaningless under debug
// codegen, so it only compiles in release (the perf_gate
// convention; budgets in BUDGETS.md).
#![cfg(not(debug_assertions))]
// v7.30.3 — the round26_mem gate's peak-tracking #[global_allocator]
// shim is intrinsically unsafe (GlobalAlloc), same per-target allow
// as spg-server's alloc_budget.
#![allow(unsafe_code)]
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

// v7.30.3 (mailrs round-26) — live-bytes + high-water tracking so
// the memory gate (round26_mem) can assert the bounded join path's
// peak. Two relaxed atomics per alloc — noise against the 10-1000×
// headroom of the timing budgets in this binary. The allocator shim
// is the one legitimate unsafe surface in this target (same pattern
// as spg-server's alloc_budget module); the file-level allow lives
// in the header attribute block.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);
// v7.33 (P4 increment 3) — allocation *count* (not bytes), so the
// proj_borrow gate can assert the projection borrow channel doesn't
// regress back to per-cell + intermediate-Row cloning. A third relaxed
// atomic per alloc, same negligible-noise rationale as the byte meters.
static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);

struct PeakTracker;

unsafe impl GlobalAlloc for PeakTracker {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            let live = LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static PEAK_TRACKING_ALLOC: PeakTracker = PeakTracker;

mod content_worker_top_n;
mod count_unseen;
mod join_reorder;
mod never_die;
mod ordered_agg;
mod plan_cache;
mod proj_borrow;
mod resident;
mod round26_mem;
mod select_where;
mod sentori_epic7_throughput;
mod stages_knn;
