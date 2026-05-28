//! v5.2.2 — background freezer thread.
//!
//! Periodically polls `Catalog::hot_tier_bytes()` and compares against
//! `ServerState::hot_tier_byte_budget` (parsed from `SPG_HOT_TIER_BYTES`
//! at startup, default 4 GiB). When the catalog-wide hot-tier byte sum
//! exceeds the budget, picks the largest-by-bytes table that has a
//! `BTree` integer-PK index and demotes a batch of its oldest rows to a
//! new cold-tier segment via `Catalog::freeze_oldest_to_cold`.
//!
//! v5.2.2 limits (relaxed in later sub-versions):
//!
//! - **Skips ticks while any TX is open.** `exec_commit` overwrites
//!   `self.catalog` from the TX slot, which would clobber any
//!   concurrent freeze. v5.2.3+ coordinates the commit path with
//!   freezer mutations properly.
//! - **One freeze per tick.** A large budget overshoot takes several
//!   ticks to bring under control; the operator can raise
//!   `SPG_FREEZE_BATCH_ROWS` to reclaim faster.
//! - **No WAL `freeze_commit` record.** A crash mid-freeze loses the
//!   not-yet-persisted segment on restart. v5.3's manifest closes
//!   this; v5.2.4 chaos test validates the bounded-loss semantics.
//! - **Segment persistence side-loaded.** If `db_path` is configured,
//!   the segment bytes are written to
//!   `<db_path>.spg/segments/seg_<id>.spg`; operators reload it after
//!   restart via the v5.1 `SPG_PRELOAD_COLD_SEGMENT` env var. The
//!   v5.3 manifest will automate this.

use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use spg_storage::{Catalog, DataType, FreezeReport, IndexKind};

use crate::{SHUTDOWN_FLAG, ServerState};

const DEFAULT_TICK_MS: u64 = 1000;
const DEFAULT_BATCH_ROWS: usize = 1_000;
/// Lower bound on the tick interval — sub-10 ms ticks would spin
/// the engine write lock for no real benefit.
const MIN_TICK_MS: u64 = 10;

/// Configuration knobs surfaced via env vars. Defaults match the
/// v5.2.2 ship gate ("safe on a fresh server, opt-in tuning for
/// stress workloads").
#[derive(Debug, Clone, Copy)]
pub(crate) struct FreezerConfig {
    pub(crate) tick: Duration,
    pub(crate) batch_rows: usize,
    pub(crate) disabled: bool,
}

impl FreezerConfig {
    pub(crate) fn from_env() -> Self {
        // Explicit kill-switch — useful in unit/e2e tests that don't
        // want a background thread mutating engine state under them.
        let disabled = env::var("SPG_FREEZER_DISABLE")
            .ok()
            .is_some_and(|s| !s.is_empty() && s != "0");
        let tick_ms = env::var("SPG_FREEZER_TICK_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| n >= MIN_TICK_MS)
            .unwrap_or(DEFAULT_TICK_MS);
        let batch_rows = env::var("SPG_FREEZER_BATCH_ROWS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_BATCH_ROWS);
        Self {
            tick: Duration::from_millis(tick_ms),
            batch_rows,
            disabled,
        }
    }
}

/// Spawn the background freezer. Returns `None` when the freezer is
/// disabled via `SPG_FREEZER_DISABLE` so tests that don't want
/// background mutations can opt out cleanly.
pub(crate) fn spawn(state: Arc<ServerState>) -> Option<JoinHandle<()>> {
    let config = FreezerConfig::from_env();
    if config.disabled {
        return None;
    }
    let handle = thread::Builder::new()
        .name("spg-freezer".into())
        .spawn(move || run(&state, config))
        .expect("spawn freezer thread");
    Some(handle)
}

fn run(state: &ServerState, config: FreezerConfig) {
    loop {
        if SHUTDOWN_FLAG.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(config.tick);
        if SHUTDOWN_FLAG.load(Ordering::Acquire) {
            break;
        }
        if let Err(e) = tick(state, config.batch_rows) {
            eprintln!("spg-freezer: tick error: {e}");
        }
    }
}

#[derive(Debug)]
struct FreezeTarget {
    table: String,
    index: String,
}

/// Pick the table with the largest `hot_bytes` among those that have
/// at least one `BTree` index over an integer column. Returns `None`
/// when no such table exists (empty catalog, NSW-only indices,
/// text-PK schemas, etc.) — the freezer skips the tick instead of
/// erroring.
fn pick_target(cat: &Catalog) -> Option<FreezeTarget> {
    let mut best: Option<(FreezeTarget, u64)> = None;
    for name in cat.table_names() {
        let Some(t) = cat.get(&name) else { continue };
        if t.row_count() == 0 {
            continue;
        }
        let columns = &t.schema().columns;
        let Some(idx) = t.indices().iter().find(|i| {
            matches!(i.kind, IndexKind::BTree(_))
                && i.column_position < columns.len()
                && matches!(
                    columns[i.column_position].ty,
                    DataType::SmallInt | DataType::Int | DataType::BigInt
                )
        }) else {
            continue;
        };
        let hot = t.hot_bytes();
        let candidate = FreezeTarget {
            table: name,
            index: idx.name.clone(),
        };
        match best {
            None => best = Some((candidate, hot)),
            Some((_, best_hot)) if hot > best_hot => best = Some((candidate, hot)),
            _ => {}
        }
    }
    best.map(|(t, _)| t)
}

fn tick(state: &ServerState, batch_rows: usize) -> std::io::Result<()> {
    let mut engine = state
        .engine
        .write()
        .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;

    // v5.2.2 limit: skip the tick if any TX is open. exec_commit
    // assigns the tx-slot catalog back into self.catalog at COMMIT
    // time, which would silently clobber a freeze we did mid-TX.
    // Wait for all TXs to drain.
    if engine.in_transaction() {
        return Ok(());
    }

    let used = engine.catalog().hot_tier_bytes();
    if used <= state.hot_tier_byte_budget {
        return Ok(());
    }

    let Some(target) = pick_target(engine.catalog()) else {
        return Ok(());
    };
    let row_count = engine
        .catalog()
        .get(&target.table)
        .map_or(0, spg_storage::Table::row_count);
    let to_freeze = batch_rows.min(row_count);
    if to_freeze == 0 {
        return Ok(());
    }

    // Clone-mutate-replace: mutate only the committed catalog, never
    // a TX slot. `Catalog::clone` is O(N segments) Arc bumps + O(1)
    // PV/PB structural-sharing bumps thanks to v4.39 / v4.40 / v5.1.
    let mut new_cat = engine.catalog().clone();
    let report = new_cat
        .freeze_oldest_to_cold(&target.table, &target.index, to_freeze)
        .map_err(|e| std::io::Error::other(format!("freeze: {e}")))?;
    engine.replace_catalog(new_cat);
    // Reflect the new segment count on the /metrics surface
    // (`spg_cold_segments_total`) via the Metrics gauge.
    state
        .metrics
        .cold_segments
        .store(u64::from(report.segment_id + 1), Ordering::Relaxed);
    // Persist segment bytes to disk so a restart can reload via
    // SPG_PRELOAD_COLD_SEGMENT (until v5.3 manifest automates this).
    if let Some(db_path) = state.db_path.as_deref()
        && let Err(e) = persist_segment(db_path, &report)
    {
        eprintln!("spg-freezer: segment persist failed: {e}");
    }
    eprintln!(
        "spg-freezer: froze {} rows from {}.{} into seg {} ({} B freed; hot {} → {})",
        report.frozen_rows,
        target.table,
        target.index,
        report.segment_id,
        report.bytes_freed,
        used,
        engine.catalog().hot_tier_bytes(),
    );
    Ok(())
}

/// Write a segment to `<parent>/<db_stem>.spg/segments/seg_<id>.spg`
/// via a `tmp + rename` for atomicity. Returns immediately on
/// `mkdir_all` / `write` / `rename` failure — the in-memory cold
/// tier is unaffected; only the disk-persisted reload path is
/// degraded for this segment.
fn persist_segment(db_path: &Path, report: &FreezeReport) -> std::io::Result<()> {
    let parent = db_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = db_path
        .file_stem()
        .unwrap_or_else(|| OsStr::new("db"))
        .to_string_lossy();
    let seg_dir = parent.join(format!("{stem}.spg")).join("segments");
    std::fs::create_dir_all(&seg_dir)?;
    let final_path = seg_dir.join(format!("seg_{}.spg", report.segment_id));
    let tmp_path = seg_dir.join(format!("seg_{}.spg.tmp", report.segment_id));
    std::fs::write(&tmp_path, &report.segment_bytes)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_match_v5_2_2_ship_gate() {
        // Empty env → defaults.
        // (We can't easily clear arbitrary env in unit tests without
        // a guard; just verify the constants are what we documented.)
        assert_eq!(DEFAULT_TICK_MS, 1000);
        assert_eq!(DEFAULT_BATCH_ROWS, 1_000);
        assert_eq!(MIN_TICK_MS, 10);
    }
}
