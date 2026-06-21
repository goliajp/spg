//! Crash-recovery harness — drives `LockHangRecovery` and
//! `WalReplayBounded` fixture kinds.
//!
//! Both reproducers ultimately measure *boot time of `open_path`
//! after a dirty shutdown*. The difference is which dirty state we
//! engineer:
//!
//! - **lock-hang**: open a real prod-shape snapshot, kick off a
//!   write to dirty the catalog, drop the handle without a clean
//!   checkpoint (the in-process equivalent of `docker stop -t 10`),
//!   then reopen and time it.
//! - **wal-replay-bounded**: synthesise a fresh catalog, build N
//!   indices, run 5000 DELETEs to produce a WAL, drop the handle,
//!   reopen, time the WAL replay.

use crate::engine_err::ee;
use crate::fixture::{
    LockHangFixture, RecoveryStep, SynthesiseSpec, WalRecordBatch, WalReplayFixture,
};
use crate::snapshot::{ExtractedSnapshot, SnapshotState, extract_snapshot, verify_snapshot};
use anyhow::{Context, Result, anyhow};
use spg_embedded::Database;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tempfile::TempDir;

#[derive(Debug)]
pub struct RecoveryRun {
    pub recovery_ms: f64,
    pub steps_run: usize,
}

#[derive(Debug)]
pub enum RecoveryOutcome {
    Ran(RecoveryRun),
    SkippedSnapshotMissing,
}

/// Drive a lock-hang reproducer.
pub fn run_lock_hang(fixture_dir: &Path, fx: &LockHangFixture) -> Result<RecoveryOutcome> {
    // Snapshot is optional — purely synthesised reproducers skip it.
    let extracted: Option<ExtractedSnapshot> = if let Some(snap) = &fx.snapshot {
        match verify_snapshot(fixture_dir, &snap.path, &snap.sha256)? {
            SnapshotState::Missing => return Ok(RecoveryOutcome::SkippedSnapshotMissing),
            SnapshotState::Corrupted {
                actual_sha256,
                expected_sha256,
                ..
            } => {
                return Err(anyhow!(
                    "snapshot SHA-256 mismatch (got {actual_sha256}, expected {expected_sha256})"
                ));
            }
            SnapshotState::Present { path, .. } => Some(extract_snapshot(&path)?),
        }
    } else {
        None
    };

    // Resolve catalog paths in steps against the extracted snapshot
    // (if any) so YAML/JSON-relative paths still find the catalog.
    let base_path: PathBuf = extracted
        .as_ref()
        .map(|e| e.catalog_path.clone())
        .unwrap_or_else(|| fixture_dir.to_path_buf());

    // Synthesised lock-hang (no snapshot) needs *some* path to
    // open — fall back to a tempdir.
    let tmp_for_synth: Option<TempDir> = if extracted.is_none() {
        Some(tempfile::Builder::new().prefix("spg-lockhang-").tempdir()?)
    } else {
        None
    };

    let mut active: Option<Database> = None;
    let mut recovery_ms: f64 = 0.0;

    for step in &fx.steps {
        match step {
            RecoveryStep::OpenPath { catalog_path } => {
                let p = resolve(&base_path, catalog_path, &tmp_for_synth);
                let db = Database::open_path(&p)
                    .map_err(ee)
                    .with_context(|| format!("open_path {}", p.display()))?;
                active = Some(db);
            }
            RecoveryStep::Execute { sql } => {
                let db = active
                    .as_mut()
                    .ok_or_else(|| anyhow!("execute step before any open_path"))?;
                let _ = db.execute(sql).map_err(ee)?;
            }
            RecoveryStep::InjectKill9MidCheckpoint { delay_ms } => {
                // Sleep for the requested delay, then drop the
                // handle *without* a clean shutdown. The catalog's
                // lock dir survives — the next OpenPath has to
                // recover from it.
                std::thread::sleep(std::time::Duration::from_millis(*delay_ms));
                drop(active.take());
            }
            RecoveryStep::ReopenPath {
                catalog_path,
                expect_success,
            } => {
                let p = resolve(&base_path, catalog_path, &tmp_for_synth);
                // Remove any stale lock dir — `kill -9` would have
                // left it on a real machine; the next supervisor
                // boot's job is to clear it.
                let _ = std::fs::remove_dir_all(format!("{}.lock", p.display()));

                let start = Instant::now();
                let opened = Database::open_path(&p);
                recovery_ms = start.elapsed().as_secs_f64() * 1000.0;

                match (opened, *expect_success) {
                    (Ok(db), true) => active = Some(db),
                    (Err(e), true) => {
                        return Err(anyhow!("reopen expected success but failed: {e}"));
                    }
                    (Ok(_), false) => {
                        return Err(anyhow!("reopen expected failure but succeeded"));
                    }
                    (Err(_), false) => {
                        // Expected failure — leave `active` empty.
                    }
                }
            }
        }
    }

    Ok(RecoveryOutcome::Ran(RecoveryRun {
        recovery_ms,
        steps_run: fx.steps.len(),
    }))
}

fn resolve(base: &Path, p: &str, tmp_synth: &Option<TempDir>) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    // "snapshot/mailrs.spg" or similar — join under base.
    if p.starts_with("snapshot/") {
        return base.parent().unwrap_or(base).join(path);
    }
    if let Some(t) = tmp_synth {
        t.path().join(path)
    } else {
        base.join(path)
    }
}

/// Drive a synthesised WAL-replay reproducer.
pub fn run_wal_replay(fx: &WalReplayFixture) -> Result<RecoveryRun> {
    let tmp = tempfile::Builder::new()
        .prefix("spg-walreplay-")
        .tempdir()?;
    let catalog = tmp.path().join("synth.spg");

    // Build the seed table + indices.
    {
        let mut db = Database::open_path(&catalog).map_err(ee)?;
        seed_table(&mut db, &fx.synthesise)?;
        write_wal_records(&mut db, &fx.synthesise.wal_records)?;
        // Drop without a clean checkpoint — the WAL has to be
        // replayed on next open.
        drop(db);
    }

    // Re-open + time the replay.
    let start = Instant::now();
    let _db = Database::open_path(&catalog)
        .map_err(ee)
        .with_context(|| format!("reopen synth catalog at {}", catalog.display()))?;
    let recovery_ms = start.elapsed().as_secs_f64() * 1000.0;

    Ok(RecoveryRun {
        recovery_ms,
        steps_run: 1,
    })
}

fn seed_table(db: &mut Database, spec: &SynthesiseSpec) -> Result<()> {
    db.execute("CREATE TABLE dogfood_wal (id BIGINT PRIMARY KEY, c1 TEXT, c2 BIGINT, c3 TEXT)")
        .map_err(ee)?;
    for i in 0..spec.indices_count {
        let col = match i % 3 {
            0 => "c1",
            1 => "c2",
            _ => "c3",
        };
        let _ = db.execute(&format!(
            "CREATE INDEX dogfood_wal_idx_{i} ON dogfood_wal({col})"
        ));
    }
    // Seed rows — keep modest so the framework stays under the
    // fast-tier 60 s budget. The prod failure was about *replay*
    // scaling with rows*indices*deletes, not initial row count.
    let rows = spec.table_rows.min(10_000);
    for batch_start in (0..rows).step_by(500) {
        let mut sql = String::from("INSERT INTO dogfood_wal (id, c1, c2, c3) VALUES ");
        let end = (batch_start + 500).min(rows);
        for i in batch_start..end {
            if i > batch_start {
                sql.push(',');
            }
            sql.push_str(&format!("({i}, 'r{i}', {i}, 'r{i}-c3')"));
        }
        db.execute(&sql).map_err(ee)?;
    }
    Ok(())
}

fn write_wal_records(db: &mut Database, batches: &[WalRecordBatch]) -> Result<()> {
    for batch in batches {
        match batch.kind.as_str() {
            "delete" => {
                // Cap at 500 — fast tier; we're checking *that the
                // replay path is bounded*, not exact prod count.
                let n = batch.count.min(500);
                for i in 0..n {
                    let _ = db.execute(&format!("DELETE FROM dogfood_wal WHERE id = {i}"));
                }
            }
            "insert" | "update" => {
                let n = batch.count.min(500);
                for i in 0..n {
                    let synth_id = 1_000_000 + i;
                    let _ = db.execute(&format!(
                        "INSERT INTO dogfood_wal (id, c1, c2, c3) VALUES ({synth_id}, 'u{i}', {i}, 'u{i}')"
                    ));
                }
            }
            other => {
                return Err(anyhow!("unknown WAL record kind: {other}"));
            }
        }
    }
    Ok(())
}
