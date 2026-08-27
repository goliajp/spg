//! Per-statement timing harness.
//!
//! Reads a `.sql` file, splits it on `;` (naively — SQL embedded in
//! string literals is not currently in the dogfood corpus and would
//! need a proper tokeniser), runs the warm-up iters, then runs the
//! timed iters and computes p50/p95/p99/max.
//!
//! Returned per-statement so a fixture with a mixed workload can
//! still pin the budget to the *slowest* statement.

use crate::engine_err::ee;
use anyhow::{Context, Result};
use spg_embedded::Database;
use std::time::Instant;

/// The steady-state half of a measurement. `cold` is deliberately not
/// in here: it is one execution against a freshly opened catalog, the
/// caller takes several of them across several opens, and a struct that
/// carried a single `cold_ms` beside a hundred warm samples is what let
/// a one-sample number be judged against a hard budget for a year.
#[derive(Debug, Clone)]
pub struct WarmStats {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub iters: usize,
}

/// Split a SQL file into individual statements. Naive split on `;`
/// with empty/whitespace-only chunks discarded; comments stripped
/// at the start of a line (`--`). Fine for the dogfood corpus,
/// which is hand-curated and avoids embedded semicolons.
pub fn split_sql(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in body.split(';') {
        // strip `--` line comments
        let cleaned: String = raw
            .lines()
            .filter(|line| !line.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        let trimmed = cleaned.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// One execution, timed, and nothing else. Against a freshly opened
/// catalog this is the `cold` sample; the caller reopens to take more.
///
/// # Errors
/// Whatever the engine says the statement did wrong.
pub fn time_one(db: &mut Database, sql: &str) -> Result<f64> {
    let start = Instant::now();
    let _ = db
        .execute(sql)
        .map_err(ee)
        .with_context(|| format!("execute: {}", trunc(sql)))?;
    Ok(start.elapsed().as_secs_f64() * 1000.0)
}

/// Warm-up (discarded) then the timed iters, as percentiles.
///
/// # Errors
/// Whatever the engine says the statement did wrong.
pub fn warm_stats(
    db: &mut Database,
    sql: &str,
    warmup: usize,
    measure: usize,
) -> Result<WarmStats> {
    for _ in 0..warmup {
        let _ = db.execute(sql).map_err(ee)?;
    }
    let mut samples = Vec::with_capacity(measure);
    for _ in 0..measure.max(1) {
        let start = Instant::now();
        let _ = db.execute(sql).map_err(ee)?;
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Ok(WarmStats {
        p50_ms: percentile(&samples, 0.50),
        p95_ms: percentile(&samples, 0.95),
        p99_ms: percentile(&samples, 0.99),
        max_ms: *samples.last().unwrap_or(&0.0),
        iters: samples.len(),
    })
}

/// The middle of a set of samples. Reported beside its own range,
/// because a median is exactly what hides a bimodal distribution and
/// one of these fixtures has one.
#[must_use]
pub fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    percentile(&s, 0.50)
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn trunc(s: &str) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= 80 {
        one_line
    } else {
        // Count characters, not bytes. `&one_line[..80]` panics the
        // moment a statement carries a multi-byte character within the
        // first 80 bytes, and this string is only ever built to report
        // an error -- it would replace the failure being reported with
        // one of its own.
        format!("{}…", one_line.chars().take(80).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `trunc` sliced by byte offset, so a statement carrying a
    /// multi-byte character anywhere in its first 80 bytes panicked --
    /// and it is only ever called to build the context of an error that
    /// is already being reported, so the panic would have replaced the
    /// real failure with one of its own. The same defect was found and
    /// fixed the same day in `xtests/suitelib/src/steps.rs`.
    #[test]
    fn trunc_does_not_panic_on_a_multibyte_boundary() {
        // 78 ASCII characters then a 3-byte one. The string is 81
        // bytes, so the old `len() > 80` test truncated it, and byte 80
        // lands INSIDE that last character -- which is exactly where
        // `&s[..80]` panicked. Counting characters, 79 is short enough
        // to keep whole.
        let s = format!("{}\u{4e2d}", "x".repeat(78));
        assert_eq!(s.len(), 81, "78 bytes plus a 3-byte character");
        assert_eq!(s.chars().count(), 79);
        assert_eq!(trunc(&s), s, "79 characters comes back whole");

        // One that really is too long, built so the cut itself lands
        // INSIDE a character: 78 ASCII bytes then multi-byte ones, so
        // byte 80 is the last third of the first of them. A byte slice
        // at [..80] panics here even when the length test counts
        // characters -- both halves of the fix are needed, and an
        // earlier version of this test proved only the first.
        let long = format!("{}{}", "x".repeat(78), "\u{4e2d}".repeat(10));
        assert!(
            !long.is_char_boundary(80),
            "the cut must land inside a character"
        );
        let cut = trunc(&long);
        assert!(
            cut.ends_with('\u{2026}'),
            "expected an ellipsis, got {cut:?}"
        );
        assert_eq!(cut.chars().count(), 81, "80 characters plus the ellipsis");
    }
}
