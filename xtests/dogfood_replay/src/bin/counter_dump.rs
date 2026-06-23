//! v7.37.9 Phase 0 — counter_dump.
//!
//! Read-only diagnostic. Loads a dogfood_replay fixture (snapshot
//! tarball is REQUIRED — no synthetic fallback), runs its
//! queries.sql N times, and dumps every Phase-0-relevant atomic
//! counter from spg-engine before/after the run.
//!
//! The point is to answer the v7.37.9 plan §3.3 question:
//!   "For the mailrs Class A / Class C SQL, do the DISTA / SCALARSQ
//!    / EXISTS_PULLUP / reorder fast paths actually fire?"
//!
//! Does NOT modify any planner code, does NOT polish anything.
//! Just instruments + reads + prints.
//!
//! Usage:
//!   spg-counter-dump <fixture-name> [--iters N]
//!
//! Example:
//!   cargo run --release --bin spg-counter-dump --p spg-dogfood-replay -- \
//!     mailrs-2026-06-22-track-a
//!   cargo run --release --bin spg-counter-dump --p spg-dogfood-replay -- \
//!     mailrs-2026-06-22-class-c-dashboard-cte

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};

// Need access to the snapshot helpers from the dogfood_replay crate.
// Bin-only crate means we re-declare the mod tree we need; keep it
// minimal.
#[path = "../engine_err.rs"]
mod engine_err;
#[path = "../snapshot.rs"]
mod snapshot;
#[path = "../fixture.rs"]
mod fixture;

use engine_err::ee;
use fixture::{FixtureKind, load_fixture};
use snapshot::{SnapshotState, extract_snapshot, verify_snapshot};
use spg_embedded::Database;

/// Every counter the Phase-0 plan cares about. The list is built so
/// that the names match the public statics on spg-engine and grow
/// in commits matching `v7.37.9 Phase 0 diagnostic` markers.
fn snapshot_counters() -> Vec<(&'static str, u64)> {
    use core::sync::atomic::Ordering::Relaxed;
    use spg_engine as e;
    vec![
        // join.rs — anti-join fast path (NOT EXISTS lowering)
        ("ANTI_JOIN_FAST_PATH_TRIED", e::ANTI_JOIN_FAST_PATH_TRIED.load(Relaxed)),
        ("ANTI_JOIN_FAST_PATH_FIRED", e::ANTI_JOIN_FAST_PATH_FIRED.load(Relaxed)),
        // subquery.rs — pull-up + batched scalar paths
        ("PULLUP_LIMIT1_FIRE_COUNT", e::subquery::PULLUP_LIMIT1_FIRE_COUNT.load(Relaxed)),
        ("BATCHED_SCALAR_KEYED_FIRE_COUNT", e::subquery::BATCHED_SCALAR_KEYED_FIRE_COUNT.load(Relaxed)),
        ("BATCHED_SCALAR_KEYED_PROBE_COUNT", e::subquery::BATCHED_SCALAR_KEYED_PROBE_COUNT.load(Relaxed)),
        ("BATCHED_SCALAR_FALL_THROUGH_COUNT", e::subquery::BATCHED_SCALAR_FALL_THROUGH_COUNT.load(Relaxed)),
        // subquery.rs — EXISTS pull-up: candidate + 7 bails + fire + batch
        ("EXISTS_PULLUP_CANDIDATE_COUNT", e::subquery::EXISTS_PULLUP_CANDIDATE_COUNT.load(Relaxed)),
        ("EXISTS_PULLUP_BAIL_INNER_SHAPE", e::subquery::EXISTS_PULLUP_BAIL_INNER_SHAPE.load(Relaxed)),
        ("EXISTS_PULLUP_BAIL_INNER_FROM", e::subquery::EXISTS_PULLUP_BAIL_INNER_FROM.load(Relaxed)),
        ("EXISTS_PULLUP_BAIL_NO_WHERE", e::subquery::EXISTS_PULLUP_BAIL_NO_WHERE.load(Relaxed)),
        ("EXISTS_PULLUP_BAIL_RESIDUAL_NOT_INNER", e::subquery::EXISTS_PULLUP_BAIL_RESIDUAL_NOT_INNER.load(Relaxed)),
        ("EXISTS_PULLUP_BAIL_NO_CORR", e::subquery::EXISTS_PULLUP_BAIL_NO_CORR.load(Relaxed)),
        ("EXISTS_PULLUP_BAIL_MULTICOL_DISABLED", e::subquery::EXISTS_PULLUP_BAIL_MULTICOL_DISABLED.load(Relaxed)),
        ("EXISTS_PULLUP_BAIL_UNIQUE_KEY_MISSING", e::subquery::EXISTS_PULLUP_BAIL_UNIQUE_KEY_MISSING.load(Relaxed)),
        ("EXISTS_PULLUP_FIRE_COUNT", e::subquery::EXISTS_PULLUP_FIRE_COUNT.load(Relaxed)),
        ("EXISTS_BATCH_FIRE_COUNT", e::subquery::EXISTS_BATCH_FIRE_COUNT.load(Relaxed)),
        ("EXISTS_BATCH_FALL_THROUGH_COUNT", e::subquery::EXISTS_BATCH_FALL_THROUGH_COUNT.load(Relaxed)),
        // subquery.rs — SCALARSQ (docker-fair attack)
        ("SCALARSQ_PK_PROBE_PLAN_FIRED", e::subquery::SCALARSQ_PK_PROBE_PLAN_FIRED.load(Relaxed)),
        ("SCALARSQ_PK_PROBE_FIRED", e::subquery::SCALARSQ_PK_PROBE_FIRED.load(Relaxed)),
        // v7.37.9 Phase 0 — NEW counters this commit adds
        ("REORDER_INNER_RUN_TRIED", e::reorder::REORDER_INNER_RUN_TRIED.load(Relaxed)),
        ("REORDER_INNER_RUN_FIRED", e::reorder::REORDER_INNER_RUN_FIRED.load(Relaxed)),
        ("DISTA_LITERAL_ARG2_CACHE_FIRE", e::aggregate::DISTA_LITERAL_ARG2_CACHE_FIRE.load(Relaxed)),
        ("AGGREGATE_ARRAY_AGG_ORDER_BY_FIRE", e::aggregate::AGGREGATE_ARRAY_AGG_ORDER_BY_FIRE.load(Relaxed)),
        // v7.37.9 Phase 1A-ext — per-row spec dispatch branches
        ("AGG_PER_ROW_FAST_POS", e::aggregate::AGG_PER_ROW_FAST_POS.load(Relaxed)),
        ("AGG_PER_ROW_COMPILED_HIT", e::aggregate::AGG_PER_ROW_COMPILED_HIT.load(Relaxed)),
        ("AGG_PER_ROW_COMPILED_MISS", e::aggregate::AGG_PER_ROW_COMPILED_MISS.load(Relaxed)),
        ("AGG_PER_ROW_EVAL_FALLBACK", e::aggregate::AGG_PER_ROW_EVAL_FALLBACK.load(Relaxed)),
        ("AGG_PER_ROW_COUNT_STAR_SENTINEL", e::aggregate::AGG_PER_ROW_COUNT_STAR_SENTINEL.load(Relaxed)),
        // v7.37.9 Phase 1A-ext-2 T1 — Step VM internal step-type counters
        ("STEP_VM_CALL_COUNT", e::eval::compiled::STEP_VM_CALL_COUNT.load(Relaxed)),
        ("STEP_VM_STEPS_TOTAL", e::eval::compiled::STEP_VM_STEPS_TOTAL.load(Relaxed)),
        ("STEP_VM_COLUMN_FIRE", e::eval::compiled::STEP_VM_COLUMN_FIRE.load(Relaxed)),
        ("STEP_VM_LIT_FIRE", e::eval::compiled::STEP_VM_LIT_FIRE.load(Relaxed)),
        ("STEP_VM_BINARY_FIRE", e::eval::compiled::STEP_VM_BINARY_FIRE.load(Relaxed)),
        ("STEP_VM_FUNCTION_FIRE", e::eval::compiled::STEP_VM_FUNCTION_FIRE.load(Relaxed)),
        ("STEP_VM_CAST_FIRE", e::eval::compiled::STEP_VM_CAST_FIRE.load(Relaxed)),
        ("STEP_VM_CASE_FIRE", e::eval::compiled::STEP_VM_CASE_FIRE.load(Relaxed)),
        // v7.37.9 Round 3 — heap-alloc per Step::Column / Step::Lit fire (T3 attack ROI)
        ("STEP_VM_COLUMN_HEAP_ALLOC", e::eval::compiled::STEP_VM_COLUMN_HEAP_ALLOC.load(Relaxed)),
        ("STEP_VM_LIT_HEAP_ALLOC", e::eval::compiled::STEP_VM_LIT_HEAP_ALLOC.load(Relaxed)),
    ]
}

fn delta(before: &[(&'static str, u64)], after: &[(&'static str, u64)]) -> Vec<(String, u64)> {
    before
        .iter()
        .zip(after.iter())
        .map(|(b, a)| (b.0.to_string(), a.1.saturating_sub(b.1)))
        .collect()
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        bail!("usage: spg-counter-dump <fixture-name> [--iters N]");
    }
    let fixture_name = args[1].clone();
    let mut iters: usize = 5;
    let mut i = 2;
    while i < args.len() {
        if args[i] == "--iters" {
            iters = args
                .get(i + 1)
                .and_then(|s| s.parse().ok())
                .context("--iters requires positive integer")?;
            i += 2;
        } else {
            bail!("unknown arg: {}", args[i]);
        }
    }

    // Locate the fixture (mirrors dogfood_replay's main.rs path
    // discovery).
    let workspace = workspace_root()?;
    let dir = workspace
        .join("xtests/dogfood_replay/fixtures")
        .join(&fixture_name);
    if !dir.exists() {
        bail!("fixture dir not found: {}", dir.display());
    }
    let fx = load_fixture(&dir).with_context(|| format!("load fixture {}", fixture_name))?;
    let FixtureKind::Query(q) = &fx.kind else {
        bail!("fixture {fixture_name} is not a Query kind");
    };

    eprintln!("=== counter_dump: {} ===", fx.name);
    let state = verify_snapshot(&dir, &q.snapshot.path, &q.snapshot.sha256)
        .with_context(|| format!("verify {}", q.snapshot.path))?;
    let extracted = match state {
        SnapshotState::Missing => {
            bail!(
                "snapshot tarball missing in {} (counter_dump REQUIRES the snapshot — \
                 a synthetic 24k seed wouldn't reflect mailrs's real catalog)",
                dir.display()
            );
        }
        SnapshotState::Corrupted {
            actual_sha256,
            expected_sha256,
            ..
        } => {
            bail!(
                "snapshot SHA-256 mismatch: got {actual_sha256}, expected {expected_sha256}"
            );
        }
        SnapshotState::Present { path, .. } => extract_snapshot(&path)
            .with_context(|| format!("extract {}", path.display()))?,
    };
    eprintln!("snapshot OK: {}", extracted.catalog_path.display());

    let mut db = Database::open_path(&extracted.catalog_path)
        .map_err(ee)
        .with_context(|| format!("open_path {}", extracted.catalog_path.display()))?;

    let sql_path = dir.join(&q.queries[0].file);
    let sql = std::fs::read_to_string(&sql_path)
        .with_context(|| format!("read {}", sql_path.display()))?;

    // Strip comment lines so we can split on ';' cleanly.
    let cleaned = strip_sql_comments(&sql);

    // Identify the trailing SELECT — there may be `\set` / `set` etc.
    // For mailrs Class A / C the file contains one big SELECT.
    let stmts: Vec<&str> = cleaned
        .split(';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if stmts.is_empty() {
        bail!("no SQL statements found in {}", sql_path.display());
    }
    let measure_sql = stmts.last().unwrap();
    eprintln!("measure SQL: {} chars, {} statements before it",
        measure_sql.len(), stmts.len() - 1);

    // Warm-up: run once to populate plan cache + cold-tier OS cache.
    eprintln!("warm-up run...");
    let _ = db.execute(measure_sql).map_err(ee).context("warm-up")?;

    // Snapshot counters BEFORE the measured run.
    let before = snapshot_counters();

    // Measured runs.
    let mut elapsed_ms: Vec<f64> = Vec::with_capacity(iters);
    for k in 0..iters {
        let t = Instant::now();
        let _ = db.execute(measure_sql).map_err(ee).context("measure run")?;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        elapsed_ms.push(ms);
        eprintln!("  iter {}: {:.2} ms", k + 1, ms);
    }
    let mut sorted = elapsed_ms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = sorted[sorted.len() / 2];
    let max = *sorted.last().unwrap();
    let mean = elapsed_ms.iter().sum::<f64>() / elapsed_ms.len() as f64;

    let after = snapshot_counters();
    let deltas = delta(&before, &after);

    println!();
    println!("=== counter_dump report: {} ===", fx.name);
    println!("snapshot: {}", extracted.catalog_path.display());
    println!("measure_sql length: {} bytes", measure_sql.len());
    println!();
    println!("wall-clock (N={iters}):");
    println!("  mean: {mean:.2} ms");
    println!("  p50:  {p50:.2} ms");
    println!("  max:  {max:.2} ms");
    println!();
    println!("counters (delta across {iters} measured runs):");
    let max_name = deltas.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, d) in &deltas {
        // Visual flag: zero deltas get a marker so they stand out.
        let flag = if *d == 0 { "  ZERO" } else { "" };
        println!("  {name:<width$}  {d:>10}{flag}", width = max_name);
    }
    println!();
    println!("interpretation (pre-attack hypothesis):");
    println!("  - PULLUP_LIMIT1_FIRE > 0 → correlated LIMIT 1 subquery fast path active");
    println!("  - EXISTS_PULLUP_FIRE > 0 → NOT EXISTS pull-up active (Class C ≥ 2 expected)");
    println!("  - SCALARSQ_PK_PROBE_FIRED > 0 → docker-fair SCALARSQ attack reaches this shape");
    println!("  - DISTA_LITERAL_ARG2_CACHE_FIRE > 0 → string_agg(DISTINCT col, ',') hit the v7.37.43 fast path");
    println!("  - REORDER_INNER_RUN_FIRED > 0 → planner actually permuted the join order");
    println!("  - ANY 'ZERO' marker = that attack did NOT recognise the mailrs shape (Phase 1 follow-up)");

    Ok(())
}

fn strip_sql_comments(input: &str) -> String {
    input
        .lines()
        .map(|line| {
            // Strip --... line comments while preserving inside-string
            // material isn't strictly necessary here because mailrs's
            // fixture text has no '--' inside strings.
            if let Some(idx) = line.find("--") {
                &line[..idx]
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn workspace_root() -> Result<PathBuf> {
    let mut here = std::env::current_dir()?;
    loop {
        if here.join("Cargo.toml").exists() && here.join("crates").exists() {
            return Ok(here);
        }
        if !here.pop() {
            bail!("workspace root not found from CWD");
        }
    }
}
