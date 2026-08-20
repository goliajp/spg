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

use spg_storage::{Catalog, DataType, FreezeReport, FreezeSlice, IndexKind};

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
    /// v6.7.4 — number of worker threads that run
    /// `Catalog::prepare_freeze_slice` in parallel before the
    /// coordinator's single-segment commit. `1` keeps the legacy
    /// single-threaded path. Defaults to `max(1, num_cpus() - 2)`
    /// reserving 2 cores for the I/O / dispatch threads.
    pub(crate) workers: usize,
}

impl FreezerConfig {
    pub(crate) fn from_env() -> Self {
        // Explicit kill-switch — useful in unit/e2e tests that don't
        // want a background thread mutating engine state under them.
        let disabled = env::var("SPG_FREEZER_DISABLE").is_ok_and(|s| !s.is_empty() && s != "0");
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
        let workers = env::var("SPG_FREEZER_WORKERS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or_else(default_worker_count);
        Self {
            tick: Duration::from_millis(tick_ms),
            batch_rows,
            disabled,
            workers,
        }
    }
}

/// v6.7.4 — default worker pool size: `max(1, num_cpus() - 2)`.
/// Reserves 2 cores for the request dispatch / WAL / replication
/// threads so a freeze tick doesn't starve foreground load. Caps
/// at 16 because beyond that the coordinator's k-way merge starts
/// dominating wall-time and the worker spawn overhead eats the
/// per-slice parallelism gain.
fn default_worker_count() -> usize {
    let cores = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    cores.saturating_sub(2).max(1).min(16)
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
        if let Err(e) = tick(state, config.batch_rows, config.workers) {
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

fn tick(state: &ServerState, batch_rows: usize, workers: usize) -> std::io::Result<()> {
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

    // v6.7.2 — freeze trigger:
    //   1. Any single table exceeds its own `hot_tier_bytes`
    //      override (set via `ALTER TABLE … SET hot_tier_bytes`).
    //   2. Otherwise: global hot tier exceeds
    //      `state.hot_tier_byte_budget` (env-set
    //      `SPG_HOT_TIER_BYTES`).
    // Either condition fires; the per-table check runs first so
    // operators can shrink a specific table without raising the
    // global watermark.
    let any_table_over = engine.catalog().table_names().iter().any(|n| {
        engine
            .catalog()
            .get(n)
            .and_then(|t| t.schema().hot_tier_bytes.map(|cap| t.hot_bytes() > cap))
            .unwrap_or(false)
    });
    let used = engine.catalog().hot_tier_bytes();
    if !any_table_over && used <= state.hot_tier_byte_budget {
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
    let report = freeze_with_workers(
        &mut new_cat,
        &target.table,
        &target.index,
        to_freeze,
        workers,
    )
    .map_err(|e| std::io::Error::other(format!("freeze: {e}")))?;
    engine.replace_catalog(new_cat);
    // Reflect the new segment count on the /metrics surface
    // (`spg_cold_segments_total`) via the Metrics gauge.
    // v6.7.3 — read the live count off the catalog (sparse since
    // compaction tombstones a slot without renumbering the rest).
    state.metrics.cold_segments.store(
        engine.catalog().cold_segment_count() as u64,
        Ordering::Relaxed,
    );
    // Persist segment bytes to disk so a restart can reload via
    // SPG_PRELOAD_COLD_SEGMENT (or, since v5.3.1, the manifest
    // sidecar that CHECKPOINT writes).
    if let Some(db_path) = state.db_path.as_deref() {
        // v6.6.3 — capture pre-envelope size for the metrics.
        let raw_size = report.segment_bytes.len() as u64;
        state
            .metrics
            .segment_bytes_uncompressed_in
            .fetch_add(raw_size, std::sync::atomic::Ordering::Relaxed);
        match persist_segment(db_path, &report) {
            Ok(written_path) => {
                if let Ok(written) = std::fs::metadata(&written_path) {
                    state
                        .metrics
                        .segment_bytes_compressed_out
                        .fetch_add(written.len(), std::sync::atomic::Ordering::Relaxed);
                }
                if let Ok(mut paths) = state.cold_segment_paths.lock() {
                    paths.insert(report.segment_id, written_path);
                }
            }
            Err(e) => eprintln!("spg-freezer: segment persist failed: {e}"),
        }
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

/// v6.7.4 — driver around the parallel-freezer storage API. Slices
/// the `[0, to_freeze)` row range into `workers` near-equal
/// partitions, calls `Catalog::prepare_freeze_slice` on each one
/// in a `std::thread::scope` worker, then commits the slices via
/// `Catalog::commit_freeze_slices`. `workers == 1` skips the
/// scope and runs the single-slice path inline, so the legacy
/// single-threaded freeze stays a zero-thread-spawn path.
fn freeze_with_workers(
    cat: &mut Catalog,
    table: &str,
    index: &str,
    to_freeze: usize,
    workers: usize,
) -> Result<FreezeReport, spg_storage::StorageError> {
    let workers = workers.max(1).min(to_freeze.max(1));
    let ranges = partition_range(to_freeze, workers);
    let slices: Vec<FreezeSlice> = if workers == 1 {
        ranges
            .into_iter()
            .map(|r| cat.prepare_freeze_slice(table, index, r))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        // Borrow `cat` immutably across the worker scope. Workers
        // never mutate; the commit step does, after the scope ends.
        let cat_ref: &Catalog = cat;
        std::thread::scope(|s| {
            let handles: Vec<_> = ranges
                .into_iter()
                .map(|r| s.spawn(move || cat_ref.prepare_freeze_slice(table, index, r)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("prepare_freeze_slice worker panicked"))
                .collect::<Result<Vec<_>, _>>()
        })?
    };
    cat.commit_freeze_slices(table, index, slices)
}

/// Split `n` items into `parts` contiguous half-open ranges. Earlier
/// ranges get the extra items if `n % parts != 0`. `parts == 1`
/// always returns `[0..n]`. `parts == 0` is impossible here —
/// `freeze_with_workers` clamps.
fn partition_range(n: usize, parts: usize) -> Vec<core::ops::Range<usize>> {
    let mut out = Vec::with_capacity(parts);
    let base = n / parts;
    let extra = n % parts;
    let mut start = 0;
    for i in 0..parts {
        let len = base + usize::from(i < extra);
        out.push(start..start + len);
        start += len;
    }
    out
}

/// Write a segment to `<parent>/<db_stem>.spg/segments/seg_<id>.spg`
/// via a `tmp + rename` for atomicity. Returns the final path on
/// success (v5.3.1 — the caller records it on
/// `ServerState::cold_segment_paths` so a future CHECKPOINT can
/// emit it into the manifest). Returns immediately on `mkdir_all`
/// / `write` / `rename` failure — the in-memory cold tier is
/// unaffected; only the disk-persisted reload path is degraded
/// for this segment.
fn persist_segment(db_path: &Path, report: &FreezeReport) -> std::io::Result<std::path::PathBuf> {
    let parent = db_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = db_path
        .file_stem()
        .unwrap_or_else(|| OsStr::new("db"))
        .to_string_lossy();
    let seg_dir = parent.join(format!("{stem}.spg")).join("segments");
    std::fs::create_dir_all(&seg_dir)?;
    let final_path = seg_dir.join(format!("seg_{}.spg", report.segment_id));
    let tmp_path = seg_dir.join(format!("seg_{}.spg.tmp", report.segment_id));
    // v6.6.2 — env-gated v2 envelope compression. Default `lzss`
    // wraps the v1 segment bytes in a v2 envelope (only when
    // compression is strictly smaller; wrap_v2_envelope returns
    // the v1 bytes unchanged otherwise). `none` ships v1 directly.
    let bytes_to_write = if segment_compression_enabled() {
        spg_storage::wrap_v2_envelope(report.segment_bytes.clone(), true)
    } else {
        report.segment_bytes.clone()
    };
    // v7.39 (round 147, durable-rename audit) — the embedded freezer twin
    // gained fsync(bytes) + fsync_dir in v7.38 P5.09 / v7.37.13 A1.6; the
    // server-side freeze path had neither, so a crash after rename could
    // expose a named-but-unflushed segment the catalog already references.
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(&bytes_to_write)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, &final_path)?;
    crate::fsync_dir(&seg_dir);
    Ok(final_path)
}

/// v6.6.2 — operator knob for cold-tier segment v2 envelope.
/// `SPG_SEGMENT_COMPRESSION=lzss` (default) wraps each freshly
/// persisted segment in a LZSS-compressed v2 envelope; `=none`
/// keeps the v1 layout. Cached after first call.
fn segment_compression_enabled() -> bool {
    static CHECKED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CHECKED.get_or_init(|| {
        std::env::var("SPG_SEGMENT_COMPRESSION").map_or(true, |v| !v.eq_ignore_ascii_case("none"))
    })
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
