//! v7.18 PITR P7 — end-to-end CI suite.
//!
//! Spawns the `spg` binary (the same one packaged into goliakk/spg
//! images) and exercises the full operator workflow:
//!
//!   backup-pitr → verify-pitr → archive-cmd → prune-pitr → pitr-restore
//!
//! These run as integration tests (separate process per scenario)
//! so each test can mutate its own SPG_PITR_ARCHIVE_CMD env without
//! racing the unit-test binary's mod tests. Coverage:
//!
//!   * `verify_pitr_passes_clean_after_backup`
//!     Smoke test — backup, verify with --write-missing-checksums,
//!     verify again clean.
//!   * `archive_cmd_success_records_ok`
//!     SPG_PITR_ARCHIVE_CMD=true → backup-pitr's summary contains
//!     `archive=ok`.
//!   * `archive_cmd_failure_records_FAILED_and_keeps_chunk`
//!     SPG_PITR_ARCHIVE_CMD=false → summary contains `archive=FAILED`
//!     and the chunk file stays on disk (PG-style: loud failure
//!     never causes data loss).
//!   * `restore_round_trip`
//!     backup → pitr-restore --to <high LSN> → assert restored row
//!     count.
//!   * `prune_drops_chunks_older_than_retention`
//!     Place two chunks with hand-crafted timestamps in a fresh
//!     backup dir; prune --retention-hours=1 keeps the recent one
//!     and removes the old one + its checksum sidecar.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn spg_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_spg"))
}

fn unique(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut p = std::env::temp_dir();
    p.push(format!(
        "spg-pitr-e2e-{}-{name}-{nanos}",
        std::process::id()
    ));
    p
}

/// Build a small file-backed SPG database with one CREATE TABLE
/// behind a checkpoint and two INSERTs in the post-checkpoint WAL,
/// so `backup-pitr` captures a non-empty WAL chunk.
fn seed_database(db_path: &Path) {
    use spg_embedded::Database;
    let mut db = Database::open_path(db_path).unwrap();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    db.checkpoint().unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    db.execute("INSERT INTO t VALUES (2)").unwrap();
    std::mem::forget(db);
    Database::force_unlock(db_path).unwrap();
}

fn run_spg(env: &[(&str, &str)], args: &[&str]) -> (i32, String, String) {
    let mut cmd = Command::new(spg_bin());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.env_remove("SPG_PITR_ARCHIVE_CMD"); // start clean
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.args(args);
    let out = cmd.output().expect("spawn spg");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn verify_pitr_passes_clean_after_backup() {
    let db = unique("smoke-db.spgdb");
    seed_database(&db);
    let bk = unique("smoke-backup");

    let (code, stdout, stderr) = run_spg(
        &[],
        &[
            "backup-pitr",
            "--src",
            db.to_str().unwrap(),
            "--dst",
            bk.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "backup-pitr failed: {stderr}");
    assert!(stdout.starts_with("OK "), "stdout: {stdout}");

    // First verify — should report Missing checksum and exit 1.
    let (code, stdout, _) = run_spg(&[], &["verify-pitr", "--dir", bk.to_str().unwrap()]);
    assert_eq!(
        code, 1,
        "expected verify to fail without checksum: {stdout}"
    );

    // Verify with --write-missing-checksums creates the file and reports clean.
    let (code, stdout, _) = run_spg(
        &[],
        &[
            "verify-pitr",
            "--dir",
            bk.to_str().unwrap(),
            "--write-missing-checksums",
        ],
    );
    assert_eq!(code, 0, "verify with --write-missing-checksums: {stdout}");

    let (code, stdout, _) = run_spg(&[], &["verify-pitr", "--dir", bk.to_str().unwrap()]);
    assert_eq!(code, 0, "second verify should be clean: {stdout}");
    assert!(stdout.contains("PASS"), "stdout: {stdout}");

    let _ = fs::remove_dir_all(&bk);
    let _ = fs::remove_file(&db);
    let _ = fs::remove_file({
        let mut p = db.clone();
        let mut name = p
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
            .unwrap_or_default();
        name.push(".wal");
        p.set_file_name(name);
        p
    });
}

#[test]
fn archive_cmd_success_records_ok() {
    let db = unique("ar-ok-db.spgdb");
    seed_database(&db);
    let bk = unique("ar-ok-backup");
    let (code, stdout, stderr) = run_spg(
        &[("SPG_PITR_ARCHIVE_CMD", "true")],
        &[
            "backup-pitr",
            "--src",
            db.to_str().unwrap(),
            "--dst",
            bk.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "backup-pitr failed: {stderr}");
    assert!(stdout.contains("archive=ok"), "stdout: {stdout}");
    let _ = fs::remove_dir_all(&bk);
    let _ = fs::remove_file(&db);
}

#[test]
fn archive_cmd_failure_records_failed_and_keeps_chunk() {
    let db = unique("ar-fail-db.spgdb");
    seed_database(&db);
    let bk = unique("ar-fail-backup");
    let (code, stdout, stderr) = run_spg(
        &[("SPG_PITR_ARCHIVE_CMD", "false")], // /bin/false
        &[
            "backup-pitr",
            "--src",
            db.to_str().unwrap(),
            "--dst",
            bk.to_str().unwrap(),
        ],
    );
    // backup-pitr still returns 0 — the chunk is local and intact;
    // archival failure is reported on stdout but does not cause
    // data loss.
    assert_eq!(code, 0, "backup-pitr exit: {stderr}");
    assert!(stdout.contains("archive=FAILED"), "stdout: {stdout}");
    let wal_dir = bk.join("wal");
    let chunks: Vec<_> = fs::read_dir(&wal_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    // v7.19 — rotation means ≥1 chunk; the invariant is "chunks
    // stay on disk despite archive failure", not an exact count.
    assert!(
        !chunks.is_empty(),
        "chunks must stay on disk on archive failure"
    );
    let _ = fs::remove_dir_all(&bk);
    let _ = fs::remove_file(&db);
}

#[test]
fn restore_round_trip() {
    let db = unique("rt-db.spgdb");
    seed_database(&db);
    let bk = unique("rt-backup");
    let (code, stdout, stderr) = run_spg(
        &[],
        &[
            "backup-pitr",
            "--src",
            db.to_str().unwrap(),
            "--dst",
            bk.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "backup-pitr: {stderr}, {stdout}");

    // pitr-restore via the binary. v7.19 — pass the whole chunk
    // DIRECTORY (backup carries ≥2 chunks after the seed's
    // checkpoint rotation); pitr-restore walks them in order.
    let wal_chunks: Vec<_> = fs::read_dir(bk.join("wal"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert!(!wal_chunks.is_empty(), "backup must carry chunks");
    let target = unique("rt-target.spg");
    let (code, stdout, stderr) = run_spg(
        &[],
        &[
            "pitr-restore",
            "--snapshot",
            bk.join("snapshot.spg").to_str().unwrap(),
            "--wal",
            bk.join("wal").to_str().unwrap(),
            "--to",
            "999",
            "--target",
            target.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "pitr-restore: {stderr}, {stdout}");

    // Restored snapshot should contain 2 rows (the two INSERTs
    // replayed on top of the post-checkpoint snapshot).
    use spg_embedded::{Database, Value};
    let mut restored = Database::restore(&fs::read(&target).unwrap()).unwrap();
    let rows = restored.query("SELECT COUNT(*) FROM t").unwrap();
    let count = match &rows[0][0] {
        Value::Int(n) => i64::from(*n),
        Value::BigInt(n) => *n,
        other => panic!("{other:?}"),
    };
    assert_eq!(count, 2);

    let _ = fs::remove_dir_all(&bk);
    let _ = fs::remove_file(&target);
    let _ = fs::remove_file(&db);
}

#[test]
fn prune_drops_chunks_older_than_retention() {
    let bk = unique("prune-dir");
    let wal_dir = bk.join("wal");
    fs::create_dir_all(&wal_dir).unwrap();

    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros());
    let old_us = now_us.saturating_sub(10 * 3_600 * 1_000_000);
    let recent_us = now_us.saturating_sub(60 * 1_000_000);
    let old_chunk = wal_dir.join(format!("{old_us}_1.wal"));
    let recent_chunk = wal_dir.join(format!("{recent_us}_2.wal"));
    fs::write(&old_chunk, b"old").unwrap();
    fs::write(&recent_chunk, b"recent").unwrap();
    fs::write(wal_dir.join(format!("{old_us}_1.wal.checksum")), b"abc").unwrap();

    let (code, stdout, _) = run_spg(
        &[],
        &[
            "prune-pitr",
            "--dir",
            bk.to_str().unwrap(),
            "--retention-hours",
            "1",
        ],
    );
    assert_eq!(code, 0, "prune-pitr: {stdout}");
    assert!(
        stdout.contains("removed=1") && stdout.contains("kept=1"),
        "stdout: {stdout}"
    );
    assert!(!old_chunk.exists(), "old chunk must be removed");
    assert!(recent_chunk.exists(), "recent chunk must remain");

    let _ = fs::remove_dir_all(&bk);
}
