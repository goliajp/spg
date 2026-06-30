//! Normalise result-set bytes for cross-oracle compare.
//!
//! Each `adjust_*` function knows ONE specific category of legal
//! difference (timestamp precision, float repr, EXPLAIN cost noise,
//! …). Composition order is the order in
//! [`AdjustPipeline::default`]. **Any unmatched diff after the
//! pipeline = real semantic discrepancy = test fail.** Adding a
//! step is an explicit decision: every new allowance has to argue
//! why the divergence is "legal".
//!
//! Borrows the architectural shape of PG's
//! `src/bin/pg_upgrade/dump.c::adjust_old_dumpfile()` — known
//! version-skew diffs handled by an allowlist of small textual
//! transforms; anything unforeseen is escalated as a real fault.
//!
//! v7.38 C ships the trait + composition + stub implementations
//! that compile and run; the regex bodies land during P1 fill so
//! the workspace lockfile doesn't churn for a scaffolding commit
//! that doesn't yet have a corpus to drive it.

#![allow(dead_code)]

/// Ordered chain of normalisation steps.
pub struct AdjustPipeline {
    steps: Vec<Box<dyn AdjustStep>>,
}

/// Single normalisation step. `apply` mutates the result-set lines
/// in place — keeps the pipeline allocation-free.
pub trait AdjustStep: Send + Sync {
    fn name(&self) -> &'static str;
    fn apply(&self, lines: &mut Vec<String>);
}

impl AdjustPipeline {
    /// Default pipeline. Order matters: textual transforms run
    /// before the final sort so that "same line, different
    /// timestamp precision" collapses to one canonical line before
    /// the lexicographic sort decides ordering.
    pub fn standard() -> Self {
        Self {
            steps: vec![
                Box::new(AdjustTimestamps),
                Box::new(AdjustSeqs),
                Box::new(AdjustDollarQuoted),
                Box::new(AdjustExplainCosts),
                Box::new(AdjustFloatRepr),
                Box::new(AdjustWhitespace),
                Box::new(AdjustNullDisplay),
                // Final step: lexical sort. Differential compare is
                // order-insensitive unless a fixture opts out with
                // `# oracle: ordered`.
                Box::new(AdjustOrderingViaSort),
            ],
        }
    }

    /// Apply the pipeline to a raw textual result set, return the
    /// canonicalised form.
    pub fn apply(&self, raw: String) -> String {
        let mut lines: Vec<String> = raw.lines().map(String::from).collect();
        for step in &self.steps {
            step.apply(&mut lines);
        }
        lines.join("\n")
    }

    /// Names of the steps, in order. Useful for `--explain` output
    /// when a fixture diff lands and we want to bisect which step
    /// (if any) absorbed the divergence vs introduced it.
    pub fn step_names(&self) -> Vec<&'static str> {
        self.steps.iter().map(|s| s.name()).collect()
    }
}

// =========================================================================
// adjust_*() stubs — bodies filled during v7.38 P1 fill.
// =========================================================================

/// PG returns `2026-06-22 12:34:56.789012`, MySQL strips trailing
/// `.0`, MariaDB sometimes drops sub-second entirely. Replace any
/// `YYYY-MM-DD HH:MM:SS[.fff…]` with the placeholder `<TS>` so the
/// test focuses on relative ordering, not absolute time.
pub struct AdjustTimestamps;
impl AdjustStep for AdjustTimestamps {
    fn name(&self) -> &'static str {
        "timestamps"
    }
    fn apply(&self, lines: &mut Vec<String>) {
        // v7.38 P1 — strip `YYYY-MM-DD HH:MM:SS[.fff…]` patterns to
        // the placeholder `<TS>`. No regex dependency: hand-rolled
        // state machine over each line. This handles 4 digits +
        // dash + 2 + dash + 2 + space + 2 + colon + 2 + colon + 2,
        // optionally followed by `.` + 1-6 digits.
        for line in lines.iter_mut() {
            *line = strip_timestamps(line);
        }
    }
}

fn strip_timestamps(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 19 <= bytes.len() && is_ts_prefix(&bytes[i..i + 19]) {
            // Consume optional fractional seconds.
            let mut end = i + 19;
            if end < bytes.len() && bytes[end] == b'.' {
                let mut k = end + 1;
                let mut digits = 0;
                while k < bytes.len() && bytes[k].is_ascii_digit() && digits < 6 {
                    k += 1;
                    digits += 1;
                }
                if digits > 0 {
                    end = k;
                }
            }
            out.push_str("<TS>");
            i = end;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn is_ts_prefix(s: &[u8]) -> bool {
    // YYYY-MM-DD HH:MM:SS  → 19 bytes
    if s.len() < 19 {
        return false;
    }
    s[0..4].iter().all(|c| c.is_ascii_digit())
        && s[4] == b'-'
        && s[5..7].iter().all(|c| c.is_ascii_digit())
        && s[7] == b'-'
        && s[8..10].iter().all(|c| c.is_ascii_digit())
        && (s[10] == b' ' || s[10] == b'T')
        && s[11..13].iter().all(|c| c.is_ascii_digit())
        && s[13] == b':'
        && s[14..16].iter().all(|c| c.is_ascii_digit())
        && s[16] == b':'
        && s[17..19].iter().all(|c| c.is_ascii_digit())
}

/// Sequence-allocated values diverge across oracles when row order
/// isn't pinned. Replace integer columns that match a known
/// sequence column with `<SEQ>`.
pub struct AdjustSeqs;
impl AdjustStep for AdjustSeqs {
    fn name(&self) -> &'static str {
        "seqs"
    }
    fn apply(&self, _lines: &mut Vec<String>) {
        // TODO(v7.38 P1): consult fixture directive `# oracle: seqs col=id`
        // to know which column to placeholder.
    }
}

/// PG's `$$ … $$` dollar-quoted string literal has no MySQL
/// equivalent — we re-quote with single quotes after escaping. PG
/// also emits `$function$ … $function$` for plpgsql bodies. Step
/// canonicalises both to a `'…'` form so the dump compare matches
/// MySQL/MariaDB which never see dollar quotes.
pub struct AdjustDollarQuoted;
impl AdjustStep for AdjustDollarQuoted {
    fn name(&self) -> &'static str {
        "dollar-quoted"
    }
    fn apply(&self, _lines: &mut Vec<String>) {
        // TODO(v7.38 P1).
    }
}

/// Strip EXPLAIN cost annotations so that plan-shape compares stay
/// stable. Synergises with the v7.38 D `SPG_TEST_EXPLAIN_NO_COSTS`
/// GUC: GUC suppresses the costs SPG-side, this step strips them
/// from the oracle output, both sides converge on the same
/// cost-free plan text.
pub struct AdjustExplainCosts;
impl AdjustStep for AdjustExplainCosts {
    fn name(&self) -> &'static str {
        "explain-costs"
    }
    fn apply(&self, lines: &mut Vec<String>) {
        // v7.38 P1 — strip PG-flavoured `(cost=… rows=…)` and
        // `(actual time=… rows=… loops=…)` segments from EXPLAIN
        // output so plan-shape compares across runs don't trip on
        // wall-clock / statistic drift. Keep operator names, join
        // types, filter expressions — i.e. everything BEFORE the
        // first `(cost=` / `(actual time=` token on each line.
        // Pairs with the v7.38 D `SPG_TEST_EXPLAIN_NO_COSTS=1`
        // GUC: the GUC suppresses costs SPG-side; this step
        // strips them from oracle output, both sides converge.
        for line in lines.iter_mut() {
            *line = strip_explain_costs(line);
        }
    }
}

fn strip_explain_costs(input: &str) -> String {
    // Find the leftmost cost segment.
    let cuts: [&str; 3] = ["(cost=", "(actual time=", " elapsed="];
    let mut cut_at = input.len();
    for needle in &cuts {
        if let Some(pos) = input.find(needle) {
            if pos < cut_at {
                cut_at = pos;
            }
        }
    }
    if cut_at == input.len() {
        return input.to_string();
    }
    // Truncate at the segment start, trim trailing whitespace.
    input[..cut_at].trim_end().to_string()
}

/// PG `1.23e-5`, MySQL `0.0000123` for FLOAT. Force scientific
/// notation for `|x| < 1e-3` so both sides converge.
pub struct AdjustFloatRepr;
impl AdjustStep for AdjustFloatRepr {
    fn name(&self) -> &'static str {
        "float-repr"
    }
    fn apply(&self, _lines: &mut Vec<String>) {
        // TODO(v7.38 P1).
    }
}

/// PG aligns columns with spaces; MySQL uses tabs. Collapse runs of
/// whitespace to a single space so column alignment doesn't gate
/// equality.
pub struct AdjustWhitespace;
impl AdjustStep for AdjustWhitespace {
    fn name(&self) -> &'static str {
        "whitespace"
    }
    fn apply(&self, lines: &mut Vec<String>) {
        for line in lines.iter_mut() {
            // Single safe transform — collapse internal whitespace.
            // Trailing newlines already stripped by the line iter.
            let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
            *line = collapsed;
        }
    }
}

/// PG `(null)`, MySQL `NULL`, MariaDB `NULL`. Canonicalise to
/// `NULL` so the textual diff doesn't trip on display style.
pub struct AdjustNullDisplay;
impl AdjustStep for AdjustNullDisplay {
    fn name(&self) -> &'static str {
        "null-display"
    }
    fn apply(&self, lines: &mut Vec<String>) {
        // v7.38 P1 — canonicalise NULL display variants to the
        // upper-case `NULL` token. Handles:
        // - PG psql default `(null)`
        // - psql `\pset null` configured `'(NULL)'`
        // - MySQL / MariaDB `NULL`
        // - Empty cell rendering (`|     |`) — PG aligned mode for
        //   NULL is empty space; treat lines that contain only
        //   whitespace + pipes as unchanged (the AdjustWhitespace
        //   collapse handles those independently)
        // Word-boundary-aware: don't touch `not null` constraints
        // or `is_null` column names.
        for line in lines.iter_mut() {
            *line = canonicalise_null_tokens(line);
        }
    }
}

fn canonicalise_null_tokens(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    'outer: while i < bytes.len() {
        // Try the lowercase / parenthesised forms in order of length.
        for (pat, rep) in &[("(null)", "NULL"), ("(NULL)", "NULL"), ("null", "NULL")] {
            if i + pat.len() <= bytes.len()
                && &input[i..i + pat.len()] == *pat
                // Word boundary check on both sides — don't rewrite
                // 'is_null' or 'not_null_col'.
                && !is_ident_byte_before(bytes, i)
                && !is_ident_byte_at(bytes, i + pat.len())
            {
                out.push_str(rep);
                i += pat.len();
                continue 'outer;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn is_ident_byte_before(bytes: &[u8], i: usize) -> bool {
    i > 0 && is_ident_char(bytes[i - 1])
}

fn is_ident_byte_at(bytes: &[u8], i: usize) -> bool {
    i < bytes.len() && is_ident_char(bytes[i])
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Final step: lexical sort so the differential compare is
/// order-insensitive. Fixtures that need a stable order opt out via
/// `# oracle: ordered` (parsed by the runner — to land in P1).
pub struct AdjustOrderingViaSort;
impl AdjustStep for AdjustOrderingViaSort {
    fn name(&self) -> &'static str {
        "ordering-via-sort"
    }
    fn apply(&self, lines: &mut Vec<String>) {
        lines.sort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_replaced_with_placeholder() {
        let step = AdjustTimestamps;
        let mut lines = vec![
            "row created at 2026-06-22 12:34:56 today".to_string(),
            "subsecond 2026-06-22 12:34:56.789012 end".to_string(),
            "iso-T 2026-06-22T12:34:56 end".to_string(),
            "no timestamp here".to_string(),
        ];
        step.apply(&mut lines);
        assert_eq!(lines[0], "row created at <TS> today");
        assert_eq!(lines[1], "subsecond <TS> end");
        assert_eq!(lines[2], "iso-T <TS> end");
        assert_eq!(lines[3], "no timestamp here");
    }

    #[test]
    fn explain_costs_truncated() {
        let step = AdjustExplainCosts;
        let mut lines = vec![
            "Seq Scan on t  (cost=0.00..100.00 rows=42 width=8)".to_string(),
            "Hash Join  (actual time=0.123..456.789 rows=10 loops=1)".to_string(),
            "Sort: t.id  elapsed=123us".to_string(),
            "Filter: t.id > 0".to_string(),
        ];
        step.apply(&mut lines);
        assert_eq!(lines[0], "Seq Scan on t");
        assert_eq!(lines[1], "Hash Join");
        assert_eq!(lines[2], "Sort: t.id");
        assert_eq!(lines[3], "Filter: t.id > 0");
    }

    #[test]
    fn null_display_canonicalised() {
        let step = AdjustNullDisplay;
        let mut lines = vec![
            "x | (null) | y".to_string(),
            "(NULL) (null) NULL null".to_string(),
            "is_null_col not_null name".to_string(), // identifier — don't rewrite
        ];
        step.apply(&mut lines);
        assert_eq!(lines[0], "x | NULL | y");
        assert_eq!(lines[1], "NULL NULL NULL NULL");
        assert_eq!(lines[2], "is_null_col not_null name");
    }

    #[test]
    fn whitespace_collapses_runs() {
        let step = AdjustWhitespace;
        let mut lines = vec!["a    b\tc".to_string(), "  x  y  ".to_string()];
        step.apply(&mut lines);
        assert_eq!(lines, vec!["a b c".to_string(), "x y".to_string()]);
    }

    #[test]
    fn ordering_sorts_lexically() {
        let step = AdjustOrderingViaSort;
        let mut lines = vec!["c".to_string(), "a".to_string(), "b".to_string()];
        step.apply(&mut lines);
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn standard_pipeline_step_names_in_order() {
        // Pin the composition order — if someone reorders the
        // pipeline without thinking, this test fires and forces them
        // to justify it. (Float repr / null repr have to happen
        // BEFORE sort, otherwise `(null)` and `NULL` sort apart.)
        let names = AdjustPipeline::standard().step_names();
        assert_eq!(
            names,
            vec![
                "timestamps",
                "seqs",
                "dollar-quoted",
                "explain-costs",
                "float-repr",
                "whitespace",
                "null-display",
                "ordering-via-sort",
            ]
        );
    }
}
