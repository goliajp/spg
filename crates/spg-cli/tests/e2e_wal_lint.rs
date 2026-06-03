//! v6.10.5 — `spg wal-lint` dry-run apply.
//!
//! Validates that a captured (snapshot, WAL) pair survives a
//! dry-run replay through the engine *before* it ever reaches
//! a live server. Two scenarios:
//!
//!   1. Clean pair → `OK <n>` on stdout, exit 0.
//!   2. Schema-incompatible WAL (catalog snapshot has no
//!      mention of the table the WAL inserts into) → `FAIL …`
//!      on stderr, exit 1.

#![allow(clippy::uninlined_format_args)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use spg_engine::Engine;
use spg_storage::Catalog;

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-wal-lint-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Build a clean (snapshot, WAL) pair via the engine directly,
/// then encode the WAL records into the v3 byte stream
/// `replay_wal_bytes` expects.
fn build_pair(dir: &std::path::Path, populate_table: bool) -> (PathBuf, PathBuf) {
    let mut engine = Engine::new();
    if populate_table {
        engine.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    }
    let snapshot = engine.snapshot();
    let db = dir.join("spg.db");
    std::fs::write(&db, &snapshot).unwrap();
    // Manually craft v3 auto-commit records. The on-disk format
    // for one record is:
    //   [u32 LE (len | 0xC000_0000)] [u32 LE crc] [u8 type=0x01] [sql bytes]
    let wal = dir.join("wal.log");
    let mut wal_bytes = Vec::new();
    let sqls = [
        "INSERT INTO t VALUES (1)",
        "INSERT INTO t VALUES (2)",
    ];
    for sql in sqls {
        let payload = sql.as_bytes();
        let mut crc_buf = Vec::with_capacity(1 + payload.len());
        crc_buf.push(0x01);
        crc_buf.extend_from_slice(payload);
        let crc = spg_crypto::crc32::crc32(&crc_buf);
        let header = ((payload.len() as u32) | 0x8000_0000 | 0x4000_0000).to_le_bytes();
        wal_bytes.extend_from_slice(&header);
        wal_bytes.extend_from_slice(&crc.to_le_bytes());
        wal_bytes.push(0x01);
        wal_bytes.extend_from_slice(payload);
    }
    std::fs::write(&wal, &wal_bytes).unwrap();
    (db, wal)
}

fn run_wal_lint(wal: &std::path::Path, db: &std::path::Path) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_spg"))
        .arg("wal-lint")
        .arg(wal)
        .arg("--against-schema")
        .arg(db)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn spg");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.code().unwrap_or(-1), stdout, stderr)
}

#[test]
fn wal_lint_passes_clean_pair() {
    let dir = unique_tmpdir("ok");
    let (db, wal) = build_pair(&dir, true);
    let (code, stdout, stderr) = run_wal_lint(&wal, &db);
    assert_eq!(code, 0, "clean pair: stderr={stderr}");
    assert!(
        stdout.starts_with("OK 2"),
        "expected `OK 2` on stdout, got {stdout:?}"
    );
}

#[test]
fn wal_lint_fails_on_schema_mismatch() {
    let dir = unique_tmpdir("mismatch");
    // Snapshot has no `t` table; WAL inserts into `t` → reject.
    let (db, wal) = build_pair(&dir, false);
    let (code, _stdout, stderr) = run_wal_lint(&wal, &db);
    assert_ne!(code, 0, "schema mismatch must fail");
    assert!(
        stderr.starts_with("FAIL"),
        "expected `FAIL …` on stderr, got {stderr:?}"
    );
}

#[test]
fn wal_lint_usage_error_on_missing_args() {
    let out = Command::new(env!("CARGO_BIN_EXE_spg"))
        .arg("wal-lint")
        .stderr(Stdio::piped())
        .output()
        .expect("spawn spg");
    assert_ne!(out.status.code().unwrap_or(-1), 0);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("usage:"),
        "expected usage hint on stderr, got {stderr:?}"
    );
}

#[test]
fn wal_lint_can_validate_a_real_engine_snapshot() {
    // Bonus sanity: build the snapshot via the same engine the
    // server would, write empty WAL, and lint passes.
    let dir = unique_tmpdir("empty-wal");
    let mut engine = Engine::new();
    engine.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let snapshot = engine.snapshot();
    let db = dir.join("spg.db");
    let mut f = std::fs::File::create(&db).unwrap();
    f.write_all(&snapshot).unwrap();
    let wal = dir.join("wal.log");
    std::fs::write(&wal, b"").unwrap();
    let (code, stdout, stderr) = run_wal_lint(&wal, &db);
    assert_eq!(code, 0, "empty WAL must lint clean: stderr={stderr}");
    assert!(stdout.starts_with("OK 0"));
    // Bonus: catalog deserialise round-trips at this version.
    let _ = Catalog::deserialize(&snapshot).expect("v12 deserialise");
}
