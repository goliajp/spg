//! What a wall-clock reading means.
//!
//! v7.39.13 — this lived inline in `main`, where nothing could reach
//! it, and it was wrong in two ways at once for as long as it stood
//! there. Both defects are pinned in `tests` below with the readings
//! that exposed them.

/// Over budget AND over this many times the step's own recent median
/// at the same band means the STEP got slower, not the host.
///
/// A fixed budget alone cannot tell those apart: the same commit
/// produced 35 s and 1,989,963 ms for `unit-affected` on this host,
/// and in the slow run `fmt` — fixed work every time — read 2,554 ms
/// against 2,300 ms in the fast one. So the machine was busy and no
/// step had changed. Three attempts to sense that condition directly
/// were refuted by measurement: a process-spawn probe moved 1.2x under
/// twelve spinners, a parallel-CPU probe 1.1-1.4x. A step's own past
/// on the same host is the only baseline that holds the machine
/// constant.
pub const SLOWDOWN_FACTOR: u128 = 2;

/// Over this many times the budget is red whatever the history says.
///
/// The history-relative test alone has no floor: two bad runs make the
/// bad number the new normal, which is exactly how `e2e` came to read
/// 3,911 s against a 480 s budget and report `pass` — the 2,655 s
/// median it was measured against had been recorded while the same
/// defect was live.
///
/// 4x is where this host's two populations separate with room on both
/// sides. Healthy runs of 2026-08-20: widest step 0.4x its budget.
/// The two September runs: a general machine slowdown put `lint` at
/// 2.2x and left every other step under 0.7x, while the one step that
/// had really regressed read 8.1x. Nothing lands in between.
pub const RUNAWAY_FACTOR: u128 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Over budget, and the step's own history at this band says the
    /// host is slow rather than the step.
    HostIsSlow,
    /// Over budget with nothing at this band to compare against.
    NoHistory,
    /// Over budget and over `SLOWDOWN_FACTOR` x its own median.
    Slower,
    /// Over `RUNAWAY_FACTOR` x the budget. No history argues this down.
    Runaway,
}

impl Verdict {
    /// Whether this reading fails the tier. Every tier — a release
    /// that watches a step take eight times its budget and publishes
    /// anyway is not a gate.
    pub fn is_red(self) -> bool {
        matches!(self, Verdict::Slower | Verdict::Runaway)
    }
}

/// Judge one over-budget reading. `median_ms` is `None` when this band
/// holds no earlier run of the same step.
///
/// `Runaway` outranks `Slower`: when the history is in the same runaway
/// range it is contaminated, and deferring to it is the defect this
/// function exists to prevent.
pub fn judge(ms: u128, budget_ms: u128, median_ms: Option<u128>) -> Verdict {
    if budget_ms > 0 && ms > budget_ms * RUNAWAY_FACTOR {
        return Verdict::Runaway;
    }
    match median_ms {
        None => Verdict::NoHistory,
        Some(median) if ms > median * SLOWDOWN_FACTOR => Verdict::Slower,
        Some(_) => Verdict::HostIsSlow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reading that shipped v7.39.12 while reporting `pass`:
    /// report-prerelease-20260904005244-bacd0ab.json, step `e2e`,
    /// 3,910,900 ms against a 480,000 ms budget, judged against a
    /// median of 2,655,000 ms recorded while the same defect was live.
    #[test]
    fn a_contaminated_median_cannot_excuse_a_runaway_step() {
        assert_eq!(judge(3_910_900, 480_000, Some(2_655_000)), Verdict::Runaway);
        assert!(judge(3_910_900, 480_000, Some(2_655_000)).is_red());
    }

    /// The same reading under the verdict this replaces: within 2x its
    /// own median, so the old rule called it green. Kept as the
    /// negative control — if this ever stops being `HostIsSlow` under
    /// the median rule alone, the test above stops proving anything.
    #[test]
    fn the_median_rule_alone_would_still_call_it_green() {
        let median_only = match 3_910_900u128 > 2_655_000u128 * SLOWDOWN_FACTOR {
            true => Verdict::Slower,
            false => Verdict::HostIsSlow,
        };
        assert_eq!(median_only, Verdict::HostIsSlow);
    }

    /// A runaway with no history at all is still red. The old rule
    /// recorded it and moved on, so a fresh host had no gate.
    #[test]
    fn a_runaway_needs_no_history() {
        assert_eq!(judge(3_910_900, 480_000, None), Verdict::Runaway);
    }

    /// The general machine slowdown in that same run must NOT be red.
    /// `lint` read 717,615 ms against a 330,000 ms budget — 2.2x, the
    /// widest any healthy-but-busy step reached — and its history is in
    /// the same range because the host was busy for both.
    #[test]
    fn a_busy_host_is_not_a_regression() {
        assert_eq!(judge(717_615, 330_000, Some(583_000)), Verdict::HostIsSlow);
        assert!(!judge(717_615, 330_000, Some(583_000)).is_red());
    }

    /// A step that doubled against its own history is red even well
    /// under the runaway line — the original rule, still load-bearing.
    ///
    /// The `is_red` half is the pin that matters: the defect this
    /// module fixes was not a wrong verdict, it was a right verdict
    /// that failed `precommit` only, so the release tier printed
    /// SLOWER and published. Asserting the enum alone left that
    /// ablation green, which is how it survived two versions.
    #[test]
    fn a_step_that_doubled_against_its_own_past_is_red() {
        let v = judge(300_000, 200_000, Some(100_000));
        assert_eq!(v, Verdict::Slower);
        assert!(
            v.is_red(),
            "SLOWER must fail every tier, not just precommit"
        );
    }

    /// And the two greens must stay green, or `is_red` could pass the
    /// test above by returning true for everything.
    #[test]
    fn the_two_green_verdicts_are_not_red() {
        assert!(!Verdict::HostIsSlow.is_red());
        assert!(!Verdict::NoHistory.is_red());
    }

    /// Under budget never reaches this function, but a zero budget
    /// (a step that declares none) must not divide the run by nothing.
    #[test]
    fn a_step_without_a_budget_defers_to_its_history() {
        assert_eq!(judge(9_999_999, 0, None), Verdict::NoHistory);
    }
}
