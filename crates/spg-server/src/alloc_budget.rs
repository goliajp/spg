//! v5.5.1 per-query memory budget enforced at the `#[global_allocator]`.
//!
//! A runaway query (a cartesian join, an unbounded `IN` materialisation, a
//! degenerate `ORDER BY`) can drive the process to the OS OOM killer before
//! the row-count / wall-clock limits ever trip. This module wraps the system
//! allocator so each query thread tracks how many bytes it currently holds and
//! trips the query's existing cancel flag once it crosses `SPG_MAX_QUERY_BYTES`
//! — the engine's 256-row cancel checkpoints then bail with
//! `EngineError::Cancelled`, exactly like a wall-clock timeout.
//!
//! ## Why this is safe to call from inside `alloc`
//!
//! The three pieces of per-thread state are `const`-initialised
//! `thread_local!`s. A `const` thread-local never lazily heap-allocates on
//! first access (stable since Rust 1.59), so reading it from within
//! `GlobalAlloc::alloc` / `dealloc` cannot recurse back into the allocator.
//! A plain `thread_local!` would allocate its lazy `Key` on first touch and
//! deadlock/recurse here.
//!
//! ## Accounting model
//!
//! Per-thread **net current bytes**: `alloc` adds, `dealloc`/shrink subtract
//! (saturating). This reflects what the query *currently* holds, so a query
//! that allocates and frees scratch buffers in a tight loop (high churn, low
//! peak) is not punished — only genuine high-water usage trips the budget.
//! A cross-thread free (object allocated on the query thread, dropped
//! elsewhere) is rare during synchronous execution and the saturating
//! subtraction keeps the counter sane if it happens.
#![allow(unsafe_code)] // this module IS the global-allocator shim: GlobalAlloc is
// an unsafe trait and the cancel-flag raw pointer deref is intrinsically unsafe.
// Workspace denies unsafe_code by default; main.rs uses the same per-site allow.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

thread_local! {
    /// Net bytes the active query on this thread currently holds. `const`-init
    /// so first access never heap-allocates (no allocator recursion).
    static QUERY_BYTES: Cell<usize> = const { Cell::new(0) };
    /// This thread's ceiling. `0` = untracked: the default for every thread
    /// (freezer, flusher, accept loop) and for query threads with the budget
    /// disabled, so non-query allocation pays only one `Cell` load.
    static QUERY_LIMIT: Cell<usize> = const { Cell::new(0) };
    /// Raw pointer to the active query's cancel flag. Non-null only between
    /// `reset_query_budget` and `clear_query_budget`, which bracket the
    /// synchronous `execute_*_with_cancel` call on this same thread — so the
    /// `AtomicBool` it points at outlives every allocation made in between.
    static QUERY_CANCEL: Cell<*const AtomicBool> = const { Cell::new(core::ptr::null()) };
}

/// `GlobalAlloc` wrapper around `System` that maintains the per-thread query
/// byte counter and trips the cancel flag on overshoot.
pub struct BudgetAllocator;

unsafe impl GlobalAlloc for BudgetAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            note_alloc(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            note_alloc(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note_dealloc(layout.size());
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                note_alloc(new_size - layout.size());
            } else {
                note_dealloc(layout.size() - new_size);
            }
        }
        p
    }
}

#[inline]
fn note_alloc(size: usize) {
    // `try_with` so a thread tearing down its TLS (post-`main` dtor order)
    // silently skips instead of panicking inside the allocator.
    let _ = QUERY_LIMIT.try_with(|lim| {
        let limit = lim.get();
        if limit == 0 {
            return;
        }
        let used = QUERY_BYTES.with(|b| {
            let n = b.get().saturating_add(size);
            b.set(n);
            n
        });
        if used > limit {
            QUERY_CANCEL.with(|c| {
                let ptr = c.get();
                if !ptr.is_null() {
                    // SAFETY: non-null only between reset/clear on this thread,
                    // during which the pointed-to AtomicBool is alive.
                    unsafe { (*ptr).store(true, Ordering::Relaxed) };
                }
            });
        }
    });
}

#[inline]
fn note_dealloc(size: usize) {
    let _ = QUERY_LIMIT.try_with(|lim| {
        if lim.get() == 0 {
            return;
        }
        QUERY_BYTES.with(|b| b.set(b.get().saturating_sub(size)));
    });
}

/// Bind this thread's allocator budget to a query: zero the counter, install
/// the ceiling (`0` = unlimited), and point at the query's cancel flag. Call
/// immediately before a synchronous `execute_*_with_cancel` on this thread.
pub fn reset_query_budget(limit_bytes: usize, cancel: &AtomicBool) {
    QUERY_BYTES.with(|b| b.set(0));
    QUERY_LIMIT.with(|l| l.set(limit_bytes));
    QUERY_CANCEL.with(|c| c.set(core::ptr::from_ref(cancel)));
}

/// Stop tracking after the query returns and drop the now-dangling cancel
/// pointer. Must run on the same thread that called `reset_query_budget`.
pub fn clear_query_budget() {
    QUERY_LIMIT.with(|l| l.set(0));
    QUERY_CANCEL.with(|c| c.set(core::ptr::null()));
    QUERY_BYTES.with(|b| b.set(0));
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: BudgetAllocator is the process #[global_allocator], so this test
    // thread's own incidental allocations (assert!, format!, Vec growth) also
    // flow through note_alloc and perturb QUERY_BYTES. Assertions here are
    // therefore written to be robust to that self-interference: we only assert
    // facts that hold regardless of extra background allocation (a single
    // over-limit note_alloc always trips; a disabled budget never tracks). The
    // precise end-to-end behaviour is covered by the e2e test that runs a real
    // over-budget query and expects EngineError::Cancelled.

    #[test]
    fn disabled_budget_never_tracks() {
        // limit 0 ⇒ early return before touching the counter, so even a huge
        // note_alloc leaves QUERY_BYTES at 0. Robust: incidental allocs also
        // early-return while the limit is 0.
        clear_query_budget(); // ensure this thread's limit is 0
        note_alloc(1 << 30);
        QUERY_BYTES.with(|b| assert_eq!(b.get(), 0, "untracked thread must not count"));
    }

    #[test]
    fn single_over_limit_alloc_trips_cancel_flag() {
        let flag = AtomicBool::new(false);
        reset_query_budget(1024, &flag);
        note_alloc(4096); // 4096 > 1024 in one shot ⇒ trips regardless of churn
        let tripped = flag.load(Ordering::Relaxed);
        clear_query_budget(); // clear before assert so assert's allocs aren't tracked
        assert!(
            tripped,
            "a single allocation past the ceiling must trip the flag"
        );
    }

    #[test]
    fn dealloc_subtracts_from_net() {
        // Net accounting: a matched alloc/dealloc pair nets to zero. We read the
        // counter immediately after the pair (no intervening tracked work beyond
        // the allocator's own, which a generous limit tolerates) and only assert
        // the dealloc moved the counter down from its post-alloc value.
        let flag = AtomicBool::new(false);
        reset_query_budget(usize::MAX, &flag); // huge limit: never trips
        note_alloc(8192);
        let after_alloc = QUERY_BYTES.with(Cell::get);
        note_dealloc(8192);
        let after_dealloc = QUERY_BYTES.with(Cell::get);
        clear_query_budget();
        assert!(
            after_dealloc < after_alloc,
            "dealloc must lower the net counter ({after_dealloc} !< {after_alloc})"
        );
        assert!(!flag.load(Ordering::Relaxed), "huge limit must never trip");
    }
}
