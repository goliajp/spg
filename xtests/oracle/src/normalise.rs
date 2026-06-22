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
    fn name(&self) -> &'static str { "timestamps" }
    fn apply(&self, _lines: &mut Vec<String>) {
        // TODO(v7.38 P1): regex `\b\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}(\.\d{1,6})?\b`
        // -> "<TS>". Stub keeps the runner shape; no-op = identity, so
        // any test that depends on this normalisation will fail loud
        // until the body lands.
    }
}

/// Sequence-allocated values diverge across oracles when row order
/// isn't pinned. Replace integer columns that match a known
/// sequence column with `<SEQ>`.
pub struct AdjustSeqs;
impl AdjustStep for AdjustSeqs {
    fn name(&self) -> &'static str { "seqs" }
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
    fn name(&self) -> &'static str { "dollar-quoted" }
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
    fn name(&self) -> &'static str { "explain-costs" }
    fn apply(&self, _lines: &mut Vec<String>) {
        // TODO(v7.38 P1): drop `(cost=…)` / `(rows=…)` / `(actual time=…)`
        // segments. Keep operator names, join types, filter expressions.
    }
}

/// PG `1.23e-5`, MySQL `0.0000123` for FLOAT. Force scientific
/// notation for `|x| < 1e-3` so both sides converge.
pub struct AdjustFloatRepr;
impl AdjustStep for AdjustFloatRepr {
    fn name(&self) -> &'static str { "float-repr" }
    fn apply(&self, _lines: &mut Vec<String>) {
        // TODO(v7.38 P1).
    }
}

/// PG aligns columns with spaces; MySQL uses tabs. Collapse runs of
/// whitespace to a single space so column alignment doesn't gate
/// equality.
pub struct AdjustWhitespace;
impl AdjustStep for AdjustWhitespace {
    fn name(&self) -> &'static str { "whitespace" }
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
    fn name(&self) -> &'static str { "null-display" }
    fn apply(&self, _lines: &mut Vec<String>) {
        // TODO(v7.38 P1).
    }
}

/// Final step: lexical sort so the differential compare is
/// order-insensitive. Fixtures that need a stable order opt out via
/// `# oracle: ordered` (parsed by the runner — to land in P1).
pub struct AdjustOrderingViaSort;
impl AdjustStep for AdjustOrderingViaSort {
    fn name(&self) -> &'static str { "ordering-via-sort" }
    fn apply(&self, lines: &mut Vec<String>) {
        lines.sort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
