//! v7.30.1 (mailrs round-24) — WAL serialisation stripped ON
//! CONFLICT from prepared INSERTs; crash replay then hit a UNIQUE
//! violation and the open path classified it as catalog corruption
//! (prod mail server refused to boot, manual WAL surgery the only
//! rescue).
//!
//! Two fixes under test:
//!
//! * ask 1 — the bind-final Display render now preserves statement
//!   semantics (ON CONFLICT et al.), so the replayed statement is
//!   the same legal no-op it was at runtime.
//! * ask 2 — a statement the engine rejects during boot replay is
//!   QUARANTINED (forensics log beside the WAL chunks) instead of
//!   bricking the catalog — which also un-bricks catalogs whose WAL
//!   was already written by ≤7.30.0 with the stripped form.

use std::io::Write;
use std::path::{Path, PathBuf};

use spg_embedded::Database;
use spg_storage::Value;
use tempfile::TempDir;

/// `<db_path>.wal/` — same convention as `read_wal_chunks` in the
/// PITR tests.
fn wal_dir_of(db_path: &Path) -> PathBuf {
    let mut wal_dir = PathBuf::from(db_path);
    let mut name = wal_dir
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".wal");
    wal_dir.set_file_name(name);
    wal_dir
}

/// Encode one v4 auto-commit SQL record exactly as ≤7.30.0 wrote
/// them (layout documented on `encode_v4_auto_commit`): the
/// quarantine test needs a record that is FRAME-valid but whose
/// statement the engine rejects on re-apply.
fn craft_v4_auto_commit(sql: &str, lsn: u64) -> Vec<u8> {
    const WAL_V2_SENTINEL: u32 = 0x8000_0000;
    const WAL_V3_FLAG: u32 = 0x4000_0000;
    const TYPE_V4_AUTO_COMMIT: u8 = 0x10;
    let payload = sql.as_bytes();
    let ts: i64 = 0;
    let mut crc_buf = Vec::with_capacity(1 + 16 + payload.len());
    crc_buf.push(TYPE_V4_AUTO_COMMIT);
    crc_buf.extend_from_slice(&lsn.to_le_bytes());
    crc_buf.extend_from_slice(&ts.to_le_bytes());
    crc_buf.extend_from_slice(payload);
    let crc = spg_crypto::crc32::crc32(&crc_buf);
    let header = ((payload.len() as u32) | WAL_V2_SENTINEL | WAL_V3_FLAG).to_le_bytes();
    let mut out = Vec::with_capacity(4 + 4 + 1 + 16 + payload.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&crc.to_le_bytes());
    out.push(TYPE_V4_AUTO_COMMIT);
    out.extend_from_slice(&lsn.to_le_bytes());
    out.extend_from_slice(&ts.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// The customer's 4-step repro: conflicted ON CONFLICT DO NOTHING
/// through the prepared (bind-final Display) path, a successful
/// write after it, SIGKILL, reopen. ≤7.30.0 refused to open.
#[test]
fn round24_prepared_on_conflict_no_op_survives_crash_replay() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("r24.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (a TEXT, b TEXT, UNIQUE(a, b))")
            .unwrap();
        db.execute("INSERT INTO t (a, b) VALUES ('x', 'y')")
            .unwrap();
        // Step 1 — routine idempotent write, runtime no-op.
        let stmt = db
            .prepare("INSERT INTO t (a, b) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .unwrap();
        db.execute_prepared(&stmt, &[Value::Text("x".into()), Value::Text("y".into())])
            .unwrap();
        // Step 2 — any successful write after it.
        db.execute("INSERT INTO t (a, b) VALUES ('p', 'q')")
            .unwrap();
        // Step 3 — non-graceful exit.
        std::mem::forget(db);
    }
    Database::force_unlock(&db_path).unwrap();
    // Step 4 — reopen. The replayed upsert must be the same legal
    // no-op it was at runtime.
    let mut db = Database::open_path(&db_path)
        .expect("round-24: crash replay of a conflicted upsert must not brick the catalog");
    let got = db.query("SELECT a, b FROM t ORDER BY a").unwrap();
    assert_eq!(
        got.len(),
        2,
        "seed row + post-conflict insert expected, got {got:?}"
    );
    assert_eq!(got[0][0], Value::Text("p".into()));
    assert_eq!(got[1][0], Value::Text("x".into()));
    // The replay must be CLEAN — if only the ask-2 quarantine were
    // in place (Display still stripping ON CONFLICT), the row count
    // would coincidentally match while the rejected record landed in
    // a quarantine file. Ask 1 means no record gets rejected at all.
    let quarantined: Vec<_> = std::fs::read_dir(wal_dir_of(&db_path))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("quarantine-"))
        })
        .collect();
    assert!(
        quarantined.is_empty(),
        "upsert replay must apply cleanly, not via quarantine: {quarantined:?}"
    );
}

/// DO UPDATE variant of the same path — the rendered clause carries
/// assignments + WHERE, and replay must reconstruct the updated row.
#[test]
fn round24_prepared_on_conflict_do_update_survives_crash_replay() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("r24u.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (a TEXT, b TEXT, UNIQUE(a))")
            .unwrap();
        db.execute("INSERT INTO t (a, b) VALUES ('x', 'old')")
            .unwrap();
        let stmt = db
            .prepare(
                "INSERT INTO t (a, b) VALUES ($1, $2) ON CONFLICT (a) DO UPDATE SET b = excluded.b",
            )
            .unwrap();
        db.execute_prepared(&stmt, &[Value::Text("x".into()), Value::Text("new".into())])
            .unwrap();
        std::mem::forget(db);
    }
    Database::force_unlock(&db_path).unwrap();
    let mut db = Database::open_path(&db_path).expect("round-24: DO UPDATE replay must not brick");
    let got = db.query("SELECT b FROM t").unwrap();
    assert_eq!(got.len(), 1, "upsert must not duplicate: {got:?}");
    assert_eq!(
        got[0][0],
        Value::Text("new".into()),
        "replay must reconstruct the DO UPDATE effect"
    );
}

/// ask 2 + the upgrade story: a WAL already written by ≤7.30.0
/// carries the STRIPPED form. 7.30.1 must quarantine the
/// unappliable record (forensics file beside the chunks), keep
/// booting, and apply every later record.
#[test]
fn round24_unappliable_wal_record_quarantines_instead_of_bricking() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("r24q.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (a TEXT, b TEXT, UNIQUE(a, b))")
            .unwrap();
        db.execute("INSERT INTO t (a, b) VALUES ('x', 'y')")
            .unwrap();
        std::mem::forget(db);
    }
    Database::force_unlock(&db_path).unwrap();
    // Splice in what a ≤7.30.0 prepared upsert left behind — a bare
    // duplicate INSERT (UNIQUE violation on replay) — followed by a
    // good record the boot must still reach.
    let wal_dir = wal_dir_of(&db_path);
    let mut chunks: Vec<PathBuf> = std::fs::read_dir(&wal_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wal"))
        .collect();
    chunks.sort();
    let last = chunks.last().expect("at least one WAL chunk");
    let mut f = std::fs::OpenOptions::new().append(true).open(last).unwrap();
    f.write_all(&craft_v4_auto_commit(
        "INSERT INTO t (a, b) VALUES ('x', 'y')",
        1_000,
    ))
    .unwrap();
    f.write_all(&craft_v4_auto_commit(
        "INSERT INTO t (a, b) VALUES ('p', 'q')",
        1_001,
    ))
    .unwrap();
    drop(f);

    let mut db = Database::open_path(&db_path)
        .expect("round-24 ask 2: a replay reject must quarantine, not brick the catalog");
    let got = db.query("SELECT a FROM t ORDER BY a").unwrap();
    assert_eq!(
        got.len(),
        2,
        "duplicate quarantined + later record applied expected, got {got:?}"
    );
    assert_eq!(got[0][0], Value::Text("p".into()));
    let quarantine_files: Vec<PathBuf> = std::fs::read_dir(&wal_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("quarantine-"))
        })
        .collect();
    assert_eq!(
        quarantine_files.len(),
        1,
        "exactly one quarantine forensics file expected: {quarantine_files:?}"
    );
    let body = std::fs::read_to_string(&quarantine_files[0]).unwrap();
    assert!(
        body.contains("INSERT INTO t (a, b) VALUES ('x', 'y')"),
        "quarantine log must carry the rejected statement: {body}"
    );
}
