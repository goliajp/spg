//! v7.19 P3 — retention sweep tests.
//!
//! `retention_sweep_once` is pub for direct testing here (the
//! looping wrapper around it is a thin sleep+poll). Each test
//! lays down chunk files with handcrafted unix_us prefixes,
//! drives one sweep, and asserts the right set survived.
//!
//! Archive-cmd success / failure paths use `true` / `false`
//! binaries which exit 0 / 1 respectively — same shape as the
//! spgctl P6 integration test.

use spg_embedded::retention_sweep_once;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

fn make_chunk(wal_dir: &PathBuf, age_secs: i64, lsn: u64) -> PathBuf {
    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros()) as i64;
    let us = now_us - age_secs * 1_000_000;
    let name = format!("{:016x}_{:016x}.wal", us.max(0) as u64, lsn);
    let path = wal_dir.join(name);
    fs::write(&path, b"dummy WAL bytes").unwrap();
    let mut cs = path.clone();
    let mut n = cs.file_name().unwrap().to_os_string();
    n.push(".checksum");
    cs.set_file_name(n);
    fs::write(&cs, b"deadbeef").unwrap();
    path
}

#[test]
fn retention_sweep_deletes_old_chunks_keeps_recent() {
    let tmp = TempDir::new().unwrap();
    let wal_dir = tmp.path().join("test.wal");
    fs::create_dir_all(&wal_dir).unwrap();

    let old = make_chunk(&wal_dir, 10 * 3600, 1); // 10 hours ago
    let recent = make_chunk(&wal_dir, 60, 2); // 60 s ago

    // 1-hour retention.
    retention_sweep_once(&wal_dir, 1, None).unwrap();

    assert!(!old.exists(), "old chunk should be removed");
    assert!(recent.exists(), "recent chunk should stay");

    let mut old_cs = old.clone();
    let mut n = old_cs.file_name().unwrap().to_os_string();
    n.push(".checksum");
    old_cs.set_file_name(n);
    assert!(!old_cs.exists(), "old checksum should be removed");
}

#[test]
fn retention_sweep_archive_success_then_delete() {
    let tmp = TempDir::new().unwrap();
    let wal_dir = tmp.path().join("test.wal");
    fs::create_dir_all(&wal_dir).unwrap();

    let old = make_chunk(&wal_dir, 10 * 3600, 1);

    // `true` always exits 0 — archive succeeds, chunk gets deleted.
    retention_sweep_once(&wal_dir, 1, Some("true")).unwrap();

    assert!(!old.exists(), "archive ok → chunk deleted");
}

#[test]
fn retention_sweep_archive_failure_keeps_chunk() {
    let tmp = TempDir::new().unwrap();
    let wal_dir = tmp.path().join("test.wal");
    fs::create_dir_all(&wal_dir).unwrap();

    let old = make_chunk(&wal_dir, 10 * 3600, 1);

    // `false` always exits 1 — archive fails, chunk MUST stay on disk
    // (PG-style loud failure; never silent data loss).
    retention_sweep_once(&wal_dir, 1, Some("false")).unwrap();

    assert!(old.exists(), "archive failure → chunk must stay on disk");
}

#[test]
fn retention_sweep_disabled_when_no_old_chunks() {
    let tmp = TempDir::new().unwrap();
    let wal_dir = tmp.path().join("test.wal");
    fs::create_dir_all(&wal_dir).unwrap();
    let recent = make_chunk(&wal_dir, 60, 1);

    retention_sweep_once(&wal_dir, 24, None).unwrap();

    assert!(recent.exists(), "fresh chunk under 24h retention stays");
}

#[test]
fn retention_sweep_handles_missing_wal_dir_gracefully() {
    let tmp = TempDir::new().unwrap();
    let missing = tmp.path().join("does-not-exist.wal");
    retention_sweep_once(&missing, 24, None).unwrap();
    // No panic, no error — same posture as prune-pitr on a fresh
    // backup dir that hasn't seen its first incremental yet.
}
