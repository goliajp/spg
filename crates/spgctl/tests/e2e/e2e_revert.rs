//! v6.10.7 — `spg revert --wal <p> --to-seq <N> --out <db>`.
//!
//! Replays the first N records of a WAL into a fresh engine
//! and writes the resulting catalog snapshot to `--out`. The
//! `--to-audit-entry` variant (resolve N from an audit-chain
//! entry hash) is STABILITY § "Out of v6.10".

use std::path::PathBuf;
use std::process::{Command, Stdio};

use spg_engine::Engine;
use spg_storage::Catalog;

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir()
        .join("spg-tests")
        .join(format!("spg-e2e-revert-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Craft a v3 WAL byte stream from the provided SQL list.
fn build_v3_wal(sqls: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for sql in sqls {
        let payload = sql.as_bytes();
        let mut crc_buf = Vec::with_capacity(1 + payload.len());
        crc_buf.push(0x01);
        crc_buf.extend_from_slice(payload);
        let crc = spg_crypto::crc32::crc32(&crc_buf);
        let header = ((payload.len() as u32) | 0x8000_0000 | 0x4000_0000).to_le_bytes();
        out.extend_from_slice(&header);
        out.extend_from_slice(&crc.to_le_bytes());
        out.push(0x01);
        out.extend_from_slice(payload);
    }
    out
}

fn run_revert(wal: &std::path::Path, to_seq: u64, out: &std::path::Path) -> (i32, String, String) {
    let cmd_out = Command::new(env!("CARGO_BIN_EXE_spg"))
        .arg("revert")
        .arg("--wal")
        .arg(wal)
        .arg("--to-seq")
        .arg(to_seq.to_string())
        .arg("--out")
        .arg(out)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn spg");
    let stdout = String::from_utf8_lossy(&cmd_out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&cmd_out.stderr).into_owned();
    (cmd_out.status.code().unwrap_or(-1), stdout, stderr)
}

#[test]
fn revert_replays_n_records_and_writes_snapshot() {
    let dir = unique_tmpdir("basic");
    let wal_path = dir.join("wal.log");
    let out_path = dir.join("out.db");
    std::fs::write(
        &wal_path,
        build_v3_wal(&[
            "CREATE TABLE t (id INT NOT NULL)",
            "INSERT INTO t VALUES (1)",
            "INSERT INTO t VALUES (2)",
            "INSERT INTO t VALUES (3)",
        ]),
    )
    .unwrap();
    // Replay first 2 records: CREATE + first INSERT only.
    let (code, stdout, stderr) = run_revert(&wal_path, 2, &out_path);
    assert_eq!(code, 0, "stderr={stderr}");
    assert!(stdout.starts_with("OK applied=2"));

    // The resulting snapshot should hold 1 row (id=1).
    let snap = std::fs::read(&out_path).unwrap();
    let cat = Catalog::deserialize(&snap).expect("deserialise");
    let t = cat.get("t").expect("table t restored");
    assert_eq!(t.rows().len(), 1);
}

#[test]
fn revert_to_seq_zero_writes_empty_snapshot() {
    let dir = unique_tmpdir("zero");
    let wal_path = dir.join("wal.log");
    let out_path = dir.join("out.db");
    std::fs::write(
        &wal_path,
        build_v3_wal(&[
            "CREATE TABLE t (id INT NOT NULL)",
            "INSERT INTO t VALUES (1)",
        ]),
    )
    .unwrap();
    let (code, stdout, _stderr) = run_revert(&wal_path, 0, &out_path);
    assert_eq!(code, 0);
    assert!(stdout.starts_with("OK applied=0"));
    let snap = std::fs::read(&out_path).unwrap();
    let cat = Catalog::deserialize(&snap).expect("deserialise");
    assert_eq!(cat.table_count(), 0, "to-seq 0 → empty catalog");
}

#[test]
fn revert_audit_entry_flag_errors_with_carveout_hint() {
    let dir = unique_tmpdir("audit");
    let wal_path = dir.join("wal.log");
    let out_path = dir.join("out.db");
    std::fs::write(&wal_path, b"").unwrap();
    let cmd_out = Command::new(env!("CARGO_BIN_EXE_spg"))
        .arg("revert")
        .arg("--wal")
        .arg(&wal_path)
        .arg("--to-audit-entry")
        .arg("3")
        .arg("--out")
        .arg(&out_path)
        .stderr(Stdio::piped())
        .output()
        .expect("spawn spg");
    assert_ne!(cmd_out.status.code().unwrap_or(-1), 0);
    let stderr = String::from_utf8_lossy(&cmd_out.stderr);
    assert!(
        stderr.contains("Out-of-v6.10"),
        "expected STABILITY carve-out hint, got {stderr:?}"
    );
}

#[test]
fn revert_to_seq_larger_than_wal_replays_all_then_stops() {
    let dir = unique_tmpdir("oversize");
    let wal_path = dir.join("wal.log");
    let out_path = dir.join("out.db");
    std::fs::write(
        &wal_path,
        build_v3_wal(&[
            "CREATE TABLE t (id INT NOT NULL)",
            "INSERT INTO t VALUES (1)",
        ]),
    )
    .unwrap();
    // Ask for 99 — there's only 2. Should replay both + stop.
    let (code, stdout, _stderr) = run_revert(&wal_path, 99, &out_path);
    assert_eq!(code, 0);
    assert!(stdout.starts_with("OK applied=2"));
    // Sanity: out.db replays cleanly back into a fresh engine.
    let snap = std::fs::read(&out_path).unwrap();
    let engine = Engine::restore_envelope(&snap).expect("restore");
    assert_eq!(engine.catalog().get("t").unwrap().rows().len(), 1);
}
