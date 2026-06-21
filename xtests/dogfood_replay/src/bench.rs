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
use std::path::Path;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct BenchResult {
    pub sql: String,
    pub cold_ms: f64,
    pub warm_p50_ms: f64,
    pub warm_p95_ms: f64,
    pub warm_p99_ms: f64,
    pub warm_max_ms: f64,
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

/// Load + measure every statement in `sql_path` against `db`.
pub fn bench_sql_file(
    db: &mut Database,
    sql_path: &Path,
    warmup_iters: usize,
    measure_iters: usize,
) -> Result<Vec<BenchResult>> {
    let body = std::fs::read_to_string(sql_path)
        .with_context(|| format!("read sql file {}", sql_path.display()))?;
    let stmts = split_sql(&body);
    let mut results = Vec::with_capacity(stmts.len());
    for sql in stmts {
        results.push(bench_one(db, &sql, warmup_iters, measure_iters)?);
    }
    Ok(results)
}

fn bench_one(db: &mut Database, sql: &str, warmup: usize, measure: usize) -> Result<BenchResult> {
    // First-iter cold time — captured separately because the
    // mailrs-2026-06-22 content-worker bug was a cold-tier read
    // amplification that only the first iter triggers.
    let cold_start = Instant::now();
    let _ = db
        .execute(sql)
        .map_err(ee)
        .with_context(|| format!("cold execute: {}", trunc(sql)))?;
    let cold_ms = cold_start.elapsed().as_secs_f64() * 1000.0;

    // Warm-up — discard times.
    for _ in 1..warmup.max(1) {
        let _ = db.execute(sql).map_err(ee)?;
    }

    // Timed iters.
    let mut samples = Vec::with_capacity(measure);
    for _ in 0..measure.max(1) {
        let start = Instant::now();
        let _ = db.execute(sql).map_err(ee)?;
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let p50 = percentile(&samples, 0.50);
    let p95 = percentile(&samples, 0.95);
    let p99 = percentile(&samples, 0.99);
    let max = *samples.last().unwrap_or(&0.0);

    Ok(BenchResult {
        sql: sql.to_string(),
        cold_ms,
        warm_p50_ms: p50,
        warm_p95_ms: p95,
        warm_p99_ms: p99,
        warm_max_ms: max,
        iters: samples.len(),
    })
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
    if one_line.len() <= 80 {
        one_line
    } else {
        format!("{}…", &one_line[..80])
    }
}
