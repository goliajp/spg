//! Cooperative query cancellation, split out of `lib.rs` (lib.rs split
//! 19). `CancelToken` is the lightweight handle threaded through every
//! scanning loop: it wraps an optional `&AtomicBool` flag (the server's
//! per-query watchdog) and an optional monotonic deadline (PG
//! `statement_timeout`), and `check()` returns `EngineError::Cancelled`
//! when either trips. `none()` is the zero-cost default for the
//! uncancellable path. Public API — `spg-server` / `spg-embedded`
//! construct tokens via `CancelToken::none().with_deadline(...)`.

use crate::EngineError;

/// v7.17.0 Phase 2.3 — monotonic time source for deadline-aware
/// cancellation (PG `statement_timeout`). Returns microseconds
/// since some host-stable monotonic origin (typically the first
/// call into `Instant::now()` on the server). The engine never
/// calls `Instant::now()` directly so the crate stays `#![no_std]`.
pub type MonotonicNowFn = fn() -> u64;

#[derive(Debug, Clone, Copy)]
struct Deadline {
    now_fn: MonotonicNowFn,
    /// Absolute deadline in `now_fn()` units (microseconds).
    deadline_us: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct CancelToken<'a> {
    flag: Option<&'a core::sync::atomic::AtomicBool>,
    // v7.17.0 Phase 2.3 — when set, every existing `cancel.check()`
    // checkpoint also fires `EngineError::Cancelled` once
    // `(now_fn)() >= deadline_us`. No new check sites, no thread
    // spawn per query — the monotonic now-fn read is a vDSO
    // `clock_gettime(CLOCK_MONOTONIC)` (~20ns) and only runs when
    // the host actually wired a deadline (statement_timeout > 0).
    deadline: Option<Deadline>,
}

impl<'a> CancelToken<'a> {
    #[must_use]
    pub const fn none() -> Self {
        Self {
            flag: None,
            deadline: None,
        }
    }

    #[must_use]
    pub const fn from_flag(f: &'a core::sync::atomic::AtomicBool) -> Self {
        Self {
            flag: Some(f),
            deadline: None,
        }
    }

    /// v7.17.0 Phase 2.3 — attach a monotonic deadline. `now_fn`
    /// must return microseconds since a stable origin; the token
    /// trips when `now_fn() >= deadline_us`. Compose with
    /// `from_flag(...)` when both a watchdog flag and a per-statement
    /// timeout are in play (e.g. server-wide `SPG_QUERY_TIMEOUT_MS`
    /// plus session `statement_timeout`); the tighter of the two
    /// wins by virtue of either signaling first.
    #[must_use]
    pub const fn with_deadline(mut self, now_fn: MonotonicNowFn, deadline_us: u64) -> Self {
        self.deadline = Some(Deadline {
            now_fn,
            deadline_us,
        });
        self
    }

    #[must_use]
    pub fn is_cancelled(self) -> bool {
        if self
            .flag
            .is_some_and(|f| f.load(core::sync::atomic::Ordering::Relaxed))
        {
            return true;
        }
        // Deadline check is the second branch so the "no timeout"
        // hot path (`deadline: None`) elides the now-fn call —
        // predicted-not-taken on the SLO INSERT loop.
        if let Some(d) = self.deadline
            && (d.now_fn)() >= d.deadline_us
        {
            return true;
        }
        false
    }

    /// Returns `Err(Cancelled)` if the token has been tripped.
    /// Used at row-loop checkpoints to bail cooperatively without
    /// scattering raw `is_cancelled` checks across the executor.
    #[inline]
    pub fn check(self) -> Result<(), EngineError> {
        if self.is_cancelled() {
            Err(EngineError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// v7.37.14 (B2.3 [PG+]) — time-budgeted cooperative cancel
    /// check. PG's `CHECK_FOR_INTERRUPTS` is per-tuple-count: the
    /// scanning loop calls it every N rows. That bounds latency to
    /// "N tuple processing time", which on a wide-row scan can
    /// stretch into seconds before a Ctrl-C is honoured.
    ///
    /// SPG goes past that with a time-budget variant: callers
    /// thread a `last_check_us` cursor through the loop and the
    /// helper guarantees the underlying full check (flag +
    /// deadline) fires at most `budget_us` after the previous one,
    /// regardless of tuple count. 100ms is the recommended budget;
    /// it bounds cancel-surface latency to that wall-clock window
    /// even on a single-tuple-takes-seconds path (large aggregate
    /// over a wide column, deep recursive CTE, etc.).
    ///
    /// `last_check_us` MUST be initialised to `0` by the caller and
    /// is updated in place when a real check runs (so the first
    /// call always falls through to a real check). With no
    /// deadline attached this method is a no-op — the budget only
    /// kicks in when there's a deadline that could trip.
    ///
    /// Hot-path overhead: one monotonic clock read (~20 ns vDSO)
    /// + one u64 subtraction. The full check fires only every
    /// budget window, so per-tuple cost stays in the single-digit
    /// nanoseconds.
    ///
    /// # Errors
    /// Same as [`Self::check`] — `EngineError::Cancelled` if the
    /// underlying flag or deadline has tripped.
    #[inline]
    pub fn check_with_budget(
        self,
        last_check_us: &mut u64,
        budget_us: u64,
    ) -> Result<(), EngineError> {
        let Some(d) = self.deadline else {
            // No deadline ⇒ no budget enforcement. The flag-only
            // path is already cheap; just call the inline check.
            return self.check();
        };
        let now = (d.now_fn)();
        // `*last_check_us == 0` is the "uninitialised" sentinel —
        // forces a real check on the first call so the caller
        // doesn't need to seed it with the current clock value
        // before the loop. After the first call last_check_us is
        // always non-zero (any non-zero monotonic value).
        if *last_check_us != 0 && now.saturating_sub(*last_check_us) < budget_us {
            return Ok(());
        }
        *last_check_us = now.max(1); // never store 0 after init
        self.check()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::sync::atomic::AtomicU64;

    // Deterministic monotonic clock for unit tests. Shared static
    // because `MonotonicNowFn` is `fn()` (no capture), so tests
    // serialise around CLOCK_LOCK to avoid stomping each other's
    // cursor when cargo-test runs them in parallel.
    static TEST_CLOCK: AtomicU64 = AtomicU64::new(0);
    static CLOCK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn reset_clock_to(value: u64) {
        TEST_CLOCK.store(value, core::sync::atomic::Ordering::SeqCst);
    }

    fn advance_clock(by_us: u64) -> u64 {
        TEST_CLOCK.fetch_add(by_us, core::sync::atomic::Ordering::SeqCst) + by_us
    }

    fn read_clock() -> u64 {
        TEST_CLOCK.load(core::sync::atomic::Ordering::SeqCst)
    }

    /// v7.37.14 (B2.3 [PG+] TDD) — without time-budget, a hot loop
    /// that doesn't call `check()` won't surface a deadline trip
    /// until the loop ends. With `check_with_budget(100ms)` the
    /// cancel fires at most one budget window after the deadline.
    #[test]
    fn v7_37_14_check_with_budget_fires_after_one_window() {
        let _g = CLOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_clock_to(0);
        // Deadline at 500_000 µs (500ms).
        let token = CancelToken::none().with_deadline(read_clock, 500_000);
        let mut last = 0u64;

        // Iterations 1-5: cheap fast-path (each advances 50ms;
        // budget=100ms so every other iter does a real check).
        // The deadline (500ms) hasn't tripped yet.
        for i in 1..=5 {
            advance_clock(50_000);
            let result = token.check_with_budget(&mut last, 100_000);
            assert!(
                result.is_ok(),
                "iter {i}: clock {}µs deadline 500_000µs — should not yet cancel",
                read_clock()
            );
        }
        assert_eq!(read_clock(), 250_000, "5 iters × 50ms = 250ms");

        // Advance past the deadline.
        advance_clock(300_000); // now 550ms, past 500ms deadline
        let result = token.check_with_budget(&mut last, 100_000);
        assert!(
            matches!(result, Err(EngineError::Cancelled)),
            "after deadline trip, budget check must surface Cancelled; got {result:?}"
        );
    }

    /// With no deadline attached, `check_with_budget` is a no-op
    /// (the cheap-path early-return is the flag-only check).
    #[test]
    fn v7_37_14_no_deadline_check_with_budget_is_cheap_flag_only() {
        let flag = AtomicU64::new(0); // unused; we use a real AtomicBool
        let _ = flag; // silence
        let token = CancelToken::none();
        let mut last = 0u64;
        // 100 budgeted checks must not produce any error.
        for _ in 0..100 {
            assert!(token.check_with_budget(&mut last, 100_000).is_ok());
        }
        // last never updated because there's no deadline → no
        // monotonic source to read.
        assert_eq!(last, 0, "no-deadline path doesn't touch last_check_us");
    }

    /// Budget is honoured: within one window, the helper does NOT
    /// re-check the deadline; once the window elapses, it does.
    /// Verifies the "skip until budget elapsed" branch.
    #[test]
    fn v7_37_14_budget_skips_within_window() {
        let _g = CLOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_clock_to(0);
        // Deadline at 100ms (already past clock=0 + small advance).
        let token = CancelToken::none().with_deadline(read_clock, 100_000);
        let mut last = 0u64;

        // First call advances clock by 30µs (still well within budget=100ms).
        // Initial last=0, so first call IS a real check → updates last.
        advance_clock(30);
        let _ = token.check_with_budget(&mut last, 100_000);
        let after_first = last;
        assert!(after_first > 0, "first call must perform real check + update");

        // Second call within budget — last must NOT change.
        advance_clock(30);
        let _ = token.check_with_budget(&mut last, 100_000);
        assert_eq!(
            last, after_first,
            "second call within budget window must skip real check"
        );

        // Advance past budget — next call DOES check + updates.
        advance_clock(150_000); // now well past 100ms budget
        let _ = token.check_with_budget(&mut last, 100_000);
        assert!(
            last > after_first,
            "after budget elapses, real check fires + last updates"
        );
    }
}
