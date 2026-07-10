// pedantic doc_markdown flags every bare ident in the comment-as-spec
// block + several proper nouns; allowing at the module level keeps
// the spec readable.
#![allow(clippy::doc_markdown)]

//! v6.2.2 — selectivity estimation over per-column statistics.
//!
//! Each selectivity function returns a fraction in `[0.0, 1.0]` —
//! the planner multiplies these against `row_count` to get
//! estimated input cardinality for each operator. v6.2.3 JOIN
//! reorder consumes these estimates; v6.2.4 EXPLAIN ANALYZE
//! surfaces them alongside the actual-rows count.
//!
//! Defaults follow PG's "no-stats" guesses so a freshly-loaded
//! table without a prior ANALYZE still gets a plausible plan:
//!
//!   - `DEFAULT_EQ      = 0.005`  — PG's `DEFAULT_EQ_SEL`
//!   - `DEFAULT_RANGE   = 0.333`  — PG's `DEFAULT_INEQ_SEL`
//!   - `DEFAULT_BETWEEN = 0.005`  — narrower than range; matches PG
//!     for `BETWEEN x AND y` without stats
//!   - `DEFAULT_LIKE    = 0.005`  — PG's `DEFAULT_MATCH_SEL`
//!
//! Histogram walks use a binary-search-based "fraction ≤ value"
//! primitive (`fraction_le_value`), giving us range estimation in
//! `O(log n_buckets)` per call. Equality keys off `n_distinct`
//! when the value lands inside the histogram range; out-of-range
//! values get an extrapolation cap so OUT-OF-RANGE predicates
//! don't collapse to zero (which would make the planner pick
//! degenerate plans like cross-products).

use alloc::string::ToString;

use spg_storage::Value;

use crate::statistics::ColumnStats;

/// PG's default selectivity for `col = constant` when no histogram
/// is available. v6.2.x can re-tune.
pub const DEFAULT_EQ: f64 = 0.005;

/// PG's default for `col <= / < / >= / > constant` without stats.
pub const DEFAULT_RANGE: f64 = 0.333;

/// PG's default for `col BETWEEN a AND b` without stats.
pub const DEFAULT_BETWEEN: f64 = 0.005;

/// PG's default for `col LIKE 'prefix%'` without stats.
pub const DEFAULT_LIKE: f64 = 0.005;

/// Floor for any selectivity to avoid degenerate zero estimates
/// (PG uses 1.0e-7; we widen to 1.0e-6 for v6.2.x and re-tune as
/// data shows up).
const MIN_SELECTIVITY: f64 = 1.0e-6;

/// `col = value`. With stats, returns `(1 / n_distinct) × (1 -
/// null_frac)` when `value` lies in the histogram range, else
/// scales down by an order of magnitude for out-of-range
/// extrapolation. Without stats, returns [`DEFAULT_EQ`].
pub fn equal(stats: Option<&ColumnStats>, value: &Value) -> f64 {
    let Some(s) = stats else {
        return DEFAULT_EQ;
    };
    if s.histogram_bounds.is_empty() || s.n_distinct == 0 {
        return DEFAULT_EQ;
    }
    let base = (1.0 - f64::from(s.null_frac)) / s.n_distinct as f64;
    let in_range = value_in_histogram_range(s, value);
    if in_range {
        base.max(MIN_SELECTIVITY).min(1.0)
    } else {
        // Out-of-range: still positive, but an order of magnitude
        // lower than the in-range guess.
        (base * 0.1).max(MIN_SELECTIVITY)
    }
}

/// `col >= low AND col <= high` (with both bounds optional). When
/// `low` is `None` the lower side is open at −∞; same for `high`
/// and +∞. `lo_incl` / `hi_incl` control whether the boundary
/// itself is included (currently a near-no-op since selectivity
/// estimation is approximate at the boundary, but kept in the
/// signature so the planner can pass the parser's intent through).
pub fn range(
    stats: Option<&ColumnStats>,
    low: Option<&Value>,
    high: Option<&Value>,
    _lo_incl: bool,
    _hi_incl: bool,
) -> f64 {
    let Some(s) = stats else {
        return match (low, high) {
            (Some(_), Some(_)) => DEFAULT_BETWEEN,
            _ => DEFAULT_RANGE,
        };
    };
    if s.histogram_bounds.is_empty() {
        return match (low, high) {
            (Some(_), Some(_)) => DEFAULT_BETWEEN,
            _ => DEFAULT_RANGE,
        };
    }
    let lo_frac = match low {
        None => 0.0,
        Some(v) => fraction_le_value(s, v),
    };
    let hi_frac = match high {
        None => 1.0,
        Some(v) => fraction_le_value(s, v),
    };
    let raw = (hi_frac - lo_frac).clamp(0.0, 1.0);
    (raw * (1.0 - f64::from(s.null_frac))).max(MIN_SELECTIVITY)
}

/// `col BETWEEN low AND high` — convenience for the inclusive
/// double-bounded shape. Equivalent to [`range`] with both
/// bounds set and inclusive.
pub fn between(stats: Option<&ColumnStats>, low: &Value, high: &Value) -> f64 {
    range(stats, Some(low), Some(high), true, true)
}

/// `col IN (v1, v2, …)`. Sums per-value equality selectivities,
/// clamped at 1.0. Without stats, returns `DEFAULT_EQ × len(values)`
/// (also clamped) — the same shape PG would produce.
pub fn in_list(stats: Option<&ColumnStats>, values: &[Value<'static>]) -> f64 {
    if values.is_empty() {
        // Empty IN list — selectivity 0 (matches no rows). Still
        // floored at MIN_SELECTIVITY so the planner doesn't see
        // a literal zero.
        return MIN_SELECTIVITY;
    }
    let total: f64 = values.iter().map(|v| equal(stats, v)).sum();
    total.clamp(MIN_SELECTIVITY, 1.0)
}

/// `col LIKE 'prefix%'` (or any single-prefix anchored pattern).
/// With stats, estimates as `range(prefix, prefix + "\u{FFFF}")`
/// on the assumption the column's natural ordering is a prefix
/// order (Text lex). Without stats, [`DEFAULT_LIKE`].
pub fn like_prefix(stats: Option<&ColumnStats>, prefix: &str) -> f64 {
    let Some(s) = stats else {
        return DEFAULT_LIKE;
    };
    if s.histogram_bounds.is_empty() {
        return DEFAULT_LIKE;
    }
    // Synthesize "prefix\u{10FFFF}" as the upper bound — any
    // string starting with the prefix sorts ≤ it. Avoids parsing
    // the prefix as a typed Value since this only applies to TEXT
    // columns in practice.
    let low_str = prefix.to_string();
    let mut high_str = prefix.to_string();
    high_str.push('\u{10FFFF}');
    let lo_frac = fraction_le_string(s, &low_str);
    let hi_frac = fraction_le_string(s, &high_str);
    let raw = (hi_frac - lo_frac).clamp(0.0, 1.0);
    (raw * (1.0 - f64::from(s.null_frac))).max(MIN_SELECTIVITY)
}

// ── histogram walk primitives ───────────────────────────────────

/// Return true when `value` lies between the histogram's first
/// and last bound (inclusive).
fn value_in_histogram_range(stats: &ColumnStats, value: &Value) -> bool {
    let lo = match stats.histogram_bounds.first() {
        Some(s) => s,
        None => return false,
    };
    let hi = stats
        .histogram_bounds
        .last()
        .expect("first present implies last present");
    let cmp_lo = value_cmp_str(value, lo);
    let cmp_hi = value_cmp_str(value, hi);
    matches!(
        cmp_lo,
        core::cmp::Ordering::Equal | core::cmp::Ordering::Greater
    ) && matches!(
        cmp_hi,
        core::cmp::Ordering::Equal | core::cmp::Ordering::Less
    )
}

/// `fraction of rows with column-value ≤ value`. Performs binary
/// search over the 101 histogram bounds; each bucket contains
/// approximately `1 / num_buckets` of the rows.
fn fraction_le_value(stats: &ColumnStats, value: &Value) -> f64 {
    if stats.histogram_bounds.is_empty() {
        return 0.5;
    }
    let n = stats.histogram_bounds.len();
    let num_buckets = (n - 1).max(1) as f64;
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = (lo + hi) / 2;
        match value_cmp_str(value, &stats.histogram_bounds[mid]) {
            core::cmp::Ordering::Less => hi = mid,
            core::cmp::Ordering::Equal | core::cmp::Ordering::Greater => lo = mid + 1,
        }
    }
    // `lo` = first bound strictly > value. Rows with column ≤
    // value are the first `lo - 1` (zero-indexed) buckets +
    // partial overlap with bucket `lo - 1`. For simplicity we
    // treat it as `(lo - 1) / num_buckets` clamped to [0, 1].
    let bound_idx = lo.saturating_sub(1);
    (bound_idx as f64 / num_buckets).clamp(0.0, 1.0)
}

/// String-keyed version of `fraction_le_value`. Used by
/// `like_prefix` which works on raw prefix strings instead of a
/// typed Value.
fn fraction_le_string(stats: &ColumnStats, key: &str) -> f64 {
    if stats.histogram_bounds.is_empty() {
        return 0.5;
    }
    let n = stats.histogram_bounds.len();
    let num_buckets = (n - 1).max(1) as f64;
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = (lo + hi) / 2;
        if stats.histogram_bounds[mid].as_str() <= key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let bound_idx = lo.saturating_sub(1);
    (bound_idx as f64 / num_buckets).clamp(0.0, 1.0)
}

/// v6.2.2 — Type-aware compare of a [`Value`] against a histogram
/// bound (always a canonical-form `String`). Tries to interpret
/// the bound in the value's expected numeric/date type first;
/// falls back to the value's own string representation for total
/// ordering. The fallback isn't strictly necessary for v6.2.2 —
/// the planner only calls selectivity with values matching the
/// column type — but keeps the function total.
fn value_cmp_str(value: &Value, bound: &str) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match value {
        Value::SmallInt(n) => bound
            .parse::<i64>()
            .map_or(Ordering::Equal, |b| i64::from(*n).cmp(&b)),
        Value::Int(n) => bound
            .parse::<i64>()
            .map_or(Ordering::Equal, |b| i64::from(*n).cmp(&b)),
        Value::BigInt(n) => bound.parse::<i64>().map_or(Ordering::Equal, |b| n.cmp(&b)),
        Value::Float(x) => bound
            .parse::<f64>()
            .ok()
            .and_then(|b| x.partial_cmp(&b))
            .unwrap_or(Ordering::Equal),
        Value::Text(s) | Value::Json(s) => s.as_ref().cmp(bound),
        Value::Bool(b) => {
            let bs = if *b { "t" } else { "f" };
            bs.cmp(bound)
        }
        // NUMERIC does NOT sort lexicographically ("10.5" < "9.5" as
        // text would mis-bucket), so parse the histogram bound back to a
        // numeric and compare by value; lex-compare only if it won't
        // parse.
        Value::Numeric { scaled, scale, .. } => match crate::numeric::parse_numeric_text(bound) {
            Some((b_scaled, b_scale)) => {
                crate::orderby::cmp_numeric(*scaled, *scale, b_scaled, b_scale)
            }
            None => crate::canonical_value_repr(value).as_str().cmp(bound),
        },
        // Date / Timestamp / Interval / Vector* live in their canonical
        // SQL form — ISO dates / timestamps sort correctly
        // lexicographically, so a direct string compare is safe.
        Value::Date(_)
        | Value::Timestamp(_)
        | Value::Interval { .. }
        | Value::Vector(_)
        | Value::Sq8Vector(_)
        | Value::HalfVector(_) => {
            // Best-effort: render the value and lex-compare. The
            // selectivity numbers for these on a typed column are
            // approximate anyway.
            crate::canonical_value_repr(value).as_str().cmp(bound)
        }
        Value::Null => {
            // NULL never participates in selectivity comparisons —
            // the planner accounts for it via null_frac. Return
            // Equal as a defensive no-op.
            Ordering::Equal
        }
        // v7.5.0 — Value is #[non_exhaustive]; future variants fall
        // back to canonical-form lex compare.
        _ => crate::canonical_value_repr(value).as_str().cmp(bound),
    }
}

// ── unit tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::statistics::ColumnStats;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn mk_int_stats(min: i64, max: i64, distinct: u64, nulls: f32) -> ColumnStats {
        // Synthesise a histogram with 101 bounds linearly spaced
        // between min and max. The exact spacing isn't critical
        // for the unit cases; `range` cares about ordering.
        let n = 100usize;
        let mut bounds: Vec<String> = Vec::with_capacity(n + 1);
        for i in 0..=n {
            let v = min + (max - min) * i as i64 / n as i64;
            bounds.push(alloc::format!("{v}"));
        }
        ColumnStats {
            null_frac: nulls,
            n_distinct: distinct,
            histogram_bounds: bounds,
        }
    }

    #[test]
    fn numeric_bound_compares_by_value_not_text() {
        use core::cmp::Ordering;
        // 10.5 vs 9.5: numerically 10.5 > 9.5, but a text-lex compare of
        // "10.5" < "9.5" (because '1' < '9') would mis-order — the bug
        // this guards. 105/scale1 = 10.5.
        let ten_five = Value::Numeric {
            scaled: 105,
            scale: 1,
            kind: spg_storage::NumericKind::Finite,
        };
        assert_eq!(value_cmp_str(&ten_five, "9.5"), Ordering::Greater);
        assert_eq!(value_cmp_str(&ten_five, "10.5"), Ordering::Equal);
        assert_eq!(value_cmp_str(&ten_five, "20"), Ordering::Less);
        // Different scales still compare by value (2.50 == 2.5).
        let two_five = Value::Numeric {
            scaled: 250,
            scale: 2,
            kind: spg_storage::NumericKind::Finite,
        };
        assert_eq!(value_cmp_str(&two_five, "2.5"), Ordering::Equal);
    }

    #[test]
    fn no_stats_returns_pg_defaults() {
        assert_eq!(equal(None, &Value::Int(1)), DEFAULT_EQ);
        assert_eq!(
            range(None, Some(&Value::Int(1)), None, true, true),
            DEFAULT_RANGE
        );
        assert_eq!(
            range(None, Some(&Value::Int(1)), Some(&Value::Int(2)), true, true),
            DEFAULT_BETWEEN
        );
        assert_eq!(like_prefix(None, "abc"), DEFAULT_LIKE);
    }

    #[test]
    fn equal_in_range_uses_n_distinct() {
        let s = mk_int_stats(0, 1000, 1000, 0.0);
        let est = equal(Some(&s), &Value::Int(500));
        // 1 / 1000 = 0.001
        assert!((est - 0.001).abs() < 1e-6, "got {est}");
    }

    #[test]
    fn equal_out_of_range_extrapolates_down() {
        let s = mk_int_stats(0, 1000, 1000, 0.0);
        let est = equal(Some(&s), &Value::Int(5000));
        // Out-of-range: (1 / 1000) × 0.1 = 0.0001
        assert!(est < 0.001, "out-of-range must shrink, got {est}");
        assert!(est >= MIN_SELECTIVITY);
    }

    #[test]
    fn range_open_low_open_high_is_full() {
        let s = mk_int_stats(0, 1000, 1000, 0.0);
        let est = range(Some(&s), None, None, true, true);
        // No bounds = full range × (1 - null_frac) = 1.0
        assert!((est - 1.0).abs() < 1e-6);
    }

    #[test]
    fn range_half_range_yields_about_half() {
        let s = mk_int_stats(0, 1000, 1000, 0.0);
        let est = range(Some(&s), None, Some(&Value::Int(500)), true, true);
        assert!((0.4..=0.6).contains(&est), "got {est}");
    }

    #[test]
    fn range_inverted_returns_min_selectivity() {
        let s = mk_int_stats(0, 1000, 1000, 0.0);
        // low > high → empty result; estimator returns MIN_SELECTIVITY
        let est = range(
            Some(&s),
            Some(&Value::Int(900)),
            Some(&Value::Int(100)),
            true,
            true,
        );
        assert!(est < 0.01);
    }

    #[test]
    fn between_inclusive_subrange_matches_bucket_share() {
        let s = mk_int_stats(0, 1000, 1000, 0.0);
        let est = between(Some(&s), &Value::Int(100), &Value::Int(200));
        // Approx 10% of the data range.
        assert!((0.05..=0.15).contains(&est), "got {est}");
    }

    #[test]
    fn in_list_sums_and_clamps() {
        let s = mk_int_stats(0, 100, 100, 0.0);
        let est = in_list(Some(&s), &[Value::Int(1), Value::Int(2), Value::Int(3)]);
        // 3 × (1 / 100) = 0.03
        assert!((est - 0.03).abs() < 1e-6);
        // Empty list → MIN_SELECTIVITY, never 0.
        assert!(in_list(Some(&s), &[]) >= MIN_SELECTIVITY);
    }

    #[test]
    fn in_list_caps_at_one() {
        let s = mk_int_stats(0, 5, 5, 0.0);
        let many: Vec<Value<'static>> = (0..50).map(Value::Int).collect();
        let est = in_list(Some(&s), &many);
        assert!(est <= 1.0);
    }

    #[test]
    fn like_prefix_estimates_range_share() {
        // Build a TEXT histogram so prefix matching has somewhere
        // to land. Values "a000" .. "z999" → 101 bounds.
        let mut bounds = Vec::with_capacity(101);
        let chars: Vec<char> = ('a'..='z').collect();
        for i in 0..=100 {
            let c = chars[i % chars.len()];
            bounds.push(alloc::format!("{c}{i:03}"));
        }
        bounds.sort();
        let s = ColumnStats {
            null_frac: 0.0,
            n_distinct: 1000,
            histogram_bounds: bounds,
        };
        let est_a = like_prefix(Some(&s), "a");
        let est_z = like_prefix(Some(&s), "z");
        // Both prefixes should yield positive selectivity less
        // than 1.
        assert!((MIN_SELECTIVITY..=1.0).contains(&est_a));
        assert!((MIN_SELECTIVITY..=1.0).contains(&est_z));
    }

    #[test]
    fn null_frac_reduces_selectivity_proportionally() {
        let s = mk_int_stats(0, 1000, 1000, 0.5);
        let est = range(Some(&s), None, Some(&Value::Int(500)), true, true);
        // Half-range × (1 - 0.5) ≈ 0.25
        assert!((0.20..=0.30).contains(&est), "got {est}");
    }
}
