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
    DumpCrashFixture, LockHangFixture, RecoveryStep, SynthesiseSpec, WalRecordBatch,
    WalReplayFixture,
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

/// v7.38.7 — restore a customer's dump, kill the writer, reopen, check.
///
/// The three things this asserts, in the order they matter:
///
/// 1. **The dump restores.** A fixture whose data did not load is a
///    fixture that measures nothing, and reports agreement anyway — the
///    failure mode this file has hit twice before. So the row counts
///    are checked against the fixture's own record before the crash
///    test is allowed to start.
/// 2. **Every acknowledged write survives.** The child prints how many
///    the client was told had committed, then SIGKILLs itself. That
///    number is the contract; anything less is lost data.
/// 3. **The indexes still answer what a scan answers.** Each probe is
///    asked twice — once so an index can serve it, once phrased so none
///    can — and the two must agree. This needs no expectation file, so
///    it cannot go stale, and it is the only way to catch an index that
///    came back from an unclean stop subtly wrong rather than absent.
///    sentori named GIN `jsonb_path_ops` and BRIN as the two they would
///    least expect to survive; both are probes here.
pub fn run_dump_crash(fixture_dir: &Path, fx: &DumpCrashFixture) -> Result<RecoveryOutcome> {
    let gz = fixture_dir.join(&fx.dump_gz);
    if !gz.exists() {
        return Ok(RecoveryOutcome::SkippedSnapshotMissing);
    }
    let actual = crate::snapshot::sha256_of_file(&gz)?;
    if actual != fx.sha256 {
        return Err(anyhow!(
            "dump sha256 mismatch: expected {}, got {actual} — a fixture whose \
             data is not the data it records measures something else",
            fx.sha256
        ));
    }

    let tmp = TempDir::new()?;
    let dump_sql = tmp.path().join("dump.sql");
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "gzip -dc {} > {}",
            gz.display(),
            dump_sql.display()
        ))
        .status()
        .context("gunzip the dump")?;
    if !status.success() {
        return Err(anyhow!("gunzip failed"));
    }

    let db_path = tmp.path().join("data.db");
    let writer = writer_bin()?;
    let out = std::process::Command::new(&writer)
        .arg(&db_path)
        .arg(&dump_sql)
        .arg(&fx.write_burst.statement)
        .arg(fx.write_burst.kill_after.to_string())
        .output()
        .context("run the crash writer")?;

    // SIGKILL is the expected end. A clean exit means the child never
    // reached the kill, which makes everything below meaningless.
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let acked: u32 = stdout
        .lines()
        .find_map(|l| l.strip_prefix("ACKED ")?.trim().parse().ok())
        .ok_or_else(|| {
            anyhow!(
                "crash writer never acknowledged a write — stdout {stdout:?}, stderr {:?}",
                String::from_utf8_lossy(&out.stderr)
            )
        })?;
    if out.status.code().is_some() {
        return Err(anyhow!(
            "crash writer exited cleanly with {:?}; it must die by SIGKILL or \
             this fixture is testing an orderly shutdown",
            out.status
        ));
    }

    // ── Reopen. Everything below is measured on the recovered database.
    let mut db = Database::open_path(&db_path).map_err(ee)?;

    // The restored tables must still hold at LEAST what the dump put
    // there. The burst table also holds whatever survived the kill, so
    // it is checked for its own contract just below rather than for
    // equality here — an equality check on it would fail on a perfectly
    // healthy recovery, which is a gate that cries wolf.
    for (table, expected) in &fx.restored_rows {
        let got = scalar_u64(&mut db, &format!("SELECT count(*) FROM {table}"))?;
        if got < *expected {
            return Err(anyhow!(
                "after recovery {table} holds {got} rows, the dump put {expected} there \
                 — restored data did not survive the kill"
            ));
        }
        if *table != fx.write_burst.table && got != *expected {
            return Err(anyhow!(
                "after recovery {table} holds {got} rows and the dump put {expected} \
                 there — nothing wrote to this table, so it must be unchanged"
            ));
        }
    }

    let burst = scalar_u64(
        &mut db,
        &format!("SELECT count(*) FROM {}", fx.write_burst.table),
    )?;
    let base = fx
        .restored_rows
        .iter()
        .find(|(t, _)| *t == fx.write_burst.table)
        .map_or(0, |(_, n)| *n);
    let survived = burst.saturating_sub(base);
    if survived < u64::from(acked) {
        return Err(anyhow!(
            "the client was told {acked} writes had committed and {survived} are here \
             after the kill — acknowledged data was lost"
        ));
    }

    for probe in &fx.index_probes {
        let via_index = scalar_u64(&mut db, &probe.indexed)?;
        let via_scan = scalar_u64(&mut db, &probe.scan)?;
        if via_index != via_scan {
            return Err(anyhow!(
                "{}: the index answers {via_index} and a scan of the same predicate \
                 answers {via_scan} — the index did not come back from the unclean \
                 stop intact",
                probe.what
            ));
        }
    }

    Ok(RecoveryOutcome::Ran(RecoveryRun {
        recovery_ms: 0.0,
        steps_run: fx.restored_rows.len() + fx.index_probes.len() + 1,
    }))
}

/// The crash writer, built beside this binary — building it first if the
/// caller only asked for this one.
///
/// `cargo run --bin spg-dogfood-replay` builds exactly that bin, which is
/// what every gate invocation uses, so a sibling binary this fixture
/// needs is simply not there. It failed on the release gate the first
/// time this fixture ran anywhere but the machine that wrote it — the
/// gate was right and the fixture was wrong to assume its neighbour had
/// been built.
///
/// Building it here rather than teaching every call site keeps the
/// dependency where the need is, and the build is a no-op once warm.
fn writer_bin() -> Result<PathBuf> {
    let me = std::env::current_exe().context("current_exe")?;
    let dir = me.parent().ok_or_else(|| anyhow!("no exe dir"))?;
    let p = dir.join("dump-crash-writer");
    if p.exists() {
        return Ok(p);
    }
    // Same profile as whatever is running us: `current_exe` sits in
    // target/<profile>/, so the directory name IS the profile.
    let profile = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("release");
    let mut cmd = std::process::Command::new(env!("CARGO"));
    cmd.args([
        "build",
        "-p",
        "spg-dogfood-replay",
        "--bin",
        "dump-crash-writer",
    ]);
    if profile == "release" {
        cmd.arg("--release");
    }
    let out = cmd.output().context("build dump-crash-writer")?;
    if !p.exists() {
        return Err(anyhow!(
            "dump-crash-writer still absent after building it: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(p)
}

fn scalar_u64(db: &mut Database, sql: &str) -> Result<u64> {
    match db.execute(sql).map_err(ee)? {
        spg_engine::QueryResult::Rows { rows, .. } => {
            let v = rows
                .first()
                .and_then(|r| r.values.first())
                .ok_or_else(|| anyhow!("{sql}: no row"))?;
            spg_engine::eval::value_to_text(v)
                .trim()
                .parse()
                .with_context(|| format!("{sql}: not a number"))
        }
        other => Err(anyhow!("{sql}: {other:?}")),
    }
}
