//! v7.39 (round 783) — poll a condition instead of sleeping a fixed
//! proxy interval.
//!
//! The pattern this replaces — `sleep(400ms); assert!(progress_made)`
//! — encodes "background work needs about 400 ms" as a constant. That
//! holds on an idle machine and breaks when the box is oversubscribed:
//! the worker gets descheduled, the assertion reads a half-finished
//! state and the suite reports a defect that is not there. Round 783
//! reproduced exactly that by running two workspace test suites at
//! once (`auto_compact_disabled_when_threshold_is_max` saw 3 segments
//! where it wanted 4).
//!
//! Polling is strictly stronger: the assertion is unchanged, the fast
//! path returns as soon as the condition holds (usually sooner than
//! the old sleep), and the generous deadline only matters when the
//! machine is slow — which is precisely when the fixed sleep lied.

use std::time::{Duration, Instant};

/// Poll `cond` every 20 ms until it returns true or `budget` expires.
/// Returns the final observation, so the caller's own assertion still
/// produces the diagnostic (`got {count}`) on failure.
pub fn wait_until(budget: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
