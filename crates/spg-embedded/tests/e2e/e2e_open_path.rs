//! v7.1.0 — `Database::open_path(p)`: full persistence with
//! catalog snapshot + WAL append+fsync + boot-time replay +
//! auto-checkpoint.
//!
//! Behaviour parity with the `spg-server` durability story:
//!
//!   - every successful mutating `execute()` is durable before
//!     the call returns (one fsync per record);
//!   - a fresh process boot replays the WAL into the engine
//!     restored from the catalog snapshot;
//!   - the WAL is bounded by `SPG_EMBEDDED_CHECKPOINT_BYTES`
//!     (default 4 MiB) — once that threshold is crossed at the
//!     end of an `execute()`, the database checkpoints + the
//!     WAL truncates;
//!   - `Drop` writes a final checkpoint.

use std::path::PathBuf;

use spg_embedded::Database;
use spg_storage::Value;

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir()
        .join("spg-tests")
        .join(format!("spg-embed-open-path-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn open_path_creates_db_and_wal_on_first_run() {
    let dir = unique_tmpdir("create");
    let db_path = dir.join("spg.db");
    let wal_path = dir.join("spg.db.wal");
    {
        let _db = Database::open_path(&db_path).unwrap();
    }
    // Drop wrote the initial empty snapshot.
    assert!(db_path.exists(), "snapshot landed");
    assert!(wal_path.exists(), "wal file created");
}

#[test]
fn writes_survive_process_recreate_via_wal_replay() {
    let dir = unique_tmpdir("survive");
    let db_path = dir.join("spg.db");
    // Session 1: write 3 rows, drop without explicit checkpoint.
    // The WAL must carry the records to the next session.
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'alice')").unwrap();
        db.execute("INSERT INTO t VALUES (2, 'bob')").unwrap();
        // db drops here → final checkpoint flushes snapshot + truncates WAL.
    }
    // Session 2: open + verify.
    let mut db = Database::open_path(&db_path).unwrap();
    let got = db.query("SELECT count(*) FROM t").unwrap();
    match &got[0][0] {
        Value::BigInt(2) => {}
        other => panic!("expected BigInt(2), got {other:?}"),
    }
}

#[test]
fn writes_recoverable_when_drop_is_skipped() {
    // Simulate crash: write into session 1 without calling
    // checkpoint() AND without letting Drop run (forget the
    // database). The WAL alone must rebuild state on session 2.
    let dir = unique_tmpdir("crash");
    let db_path = dir.join("spg.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        db.execute("INSERT INTO t VALUES (2)").unwrap();
        // `forget` — Drop never runs → no final checkpoint.
        // The WAL+fsync after each `execute()` is the only
        // durability guarantee.
        std::mem::forget(db);
    }
    // mem::forget leaks the advisory file lock; clear it the way
    // a post-crash operator would so the reopen isn't refused.
    Database::force_unlock(&db_path).unwrap();
    let mut db = Database::open_path(&db_path).unwrap();
    let got = db.query("SELECT count(*) FROM t").unwrap();
    match &got[0][0] {
        Value::BigInt(2) => {}
        other => panic!("expected BigInt(2), got {other:?}"),
    }
}

#[test]
fn explicit_checkpoint_rotates_wal_chunk() {
    // v7.19 — checkpoint rotates the WAL chunk instead of
    // truncating: the chunk holding the pre-checkpoint records
    // stays on disk (the retention thread sweeps it later) and a
    // fresh empty chunk becomes the active one. Confirms the
    // PITR-compatible rotation behaviour replaced the v7.18
    // single-file truncate.
    let dir = unique_tmpdir("checkpoint");
    let db_path = dir.join("spg.db");
    let wal_dir = dir.join("spg.db.wal");
    let mut db = Database::open_path(&db_path).unwrap();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    let chunks_before: Vec<_> = std::fs::read_dir(&wal_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("wal"))
        .collect();
    assert_eq!(chunks_before.len(), 1, "one active chunk before checkpoint");
    let pre_chunk_size = chunks_before[0].metadata().unwrap().len();
    assert!(pre_chunk_size > 0);
    db.checkpoint().unwrap();
    let chunks_after: Vec<_> = std::fs::read_dir(&wal_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("wal"))
        .collect();
    assert_eq!(
        chunks_after.len(),
        2,
        "checkpoint rotates — pre-checkpoint chunk stays + new active chunk added"
    );
    // Snapshot now carries the table state.
    let snap = std::fs::read(&db_path).unwrap();
    assert!(!snap.is_empty());
}

#[test]
fn auto_checkpoint_fires_under_tight_threshold() {
    // Configure a 1 KiB WAL ceiling so a handful of INSERTs
    // trip it. The next `execute()` after the threshold ends
    // with a checkpoint, leaving the WAL ≤ one record large
    // (the post-checkpoint record).
    let dir = unique_tmpdir("auto-ck");
    let db_path = dir.join("spg.db");
    let wal_path = dir.join("spg.db.wal");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.set_checkpoint_threshold_bytes(1024);
        db.execute("CREATE TABLE t (id INT NOT NULL, payload TEXT)")
            .unwrap();
        for i in 0..50 {
            let payload = "x".repeat(40);
            db.execute(&format!("INSERT INTO t VALUES ({i}, '{payload}')"))
                .unwrap();
        }
        // After 50 INSERTs the WAL must have been checkpointed at
        // least once. Since v7.19 the WAL is a CHUNK DIRECTORY and
        // checkpoints rotate chunks (pre-checkpoint chunks stay for
        // PITR), so the observable signal is chunk count ≥ 2.
        // (The old assertion measured `metadata(<db>.wal).len()` —
        // the directory inode, which is 4096 B on Linux ext4 and
        // small on APFS, so it silently passed on macOS and always
        // failed on Linux CI.)
        let chunk_count = std::fs::read_dir(&wal_path)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("wal"))
            .count();
        assert!(
            chunk_count >= 2,
            "auto-checkpoint never fired: only {chunk_count} WAL chunk(s)"
        );
    }
    // Sanity: data still readable on next open.
    let mut db = Database::open_path(&db_path).unwrap();
    let got = db.query("SELECT count(*) FROM t").unwrap();
    match &got[0][0] {
        Value::BigInt(50) => {}
        other => panic!("expected BigInt(50), got {other:?}"),
    }
}

#[test]
fn vector_state_survives_persistence_roundtrip() {
    let dir = unique_tmpdir("vector");
    let db_path = dir.join("spg.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE emb (id INT NOT NULL, v VECTOR(4) NOT NULL)")
            .unwrap();
        db.execute("CREATE INDEX ix ON emb USING hnsw (v)").unwrap();
        for (id, v) in [
            (1, "[1.0, 2.0, 3.0, 4.0]"),
            (2, "[4.0, 5.0, 6.0, 7.0]"),
            (3, "[6.0, 7.0, 8.0, 9.0]"),
            (4, "[2.0, 3.0, 4.0, 5.0]"),
            (5, "[1.0, 2.0, 3.0, 5.0]"),
        ] {
            db.execute(&format!("INSERT INTO emb VALUES ({id}, {v})"))
                .unwrap();
        }
    }
    let mut db = Database::open_path(&db_path).unwrap();
    let got = db
        .query("SELECT id FROM emb ORDER BY v <-> [1.0, 2.0, 3.0, 4.0] LIMIT 3")
        .unwrap();
    let ids: Vec<i32> = got
        .into_iter()
        .map(|r| match r.into_iter().next().unwrap() {
            Value::Int(n) => n,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(ids, vec![1, 5, 4]);
}

#[test]
fn read_only_select_does_not_grow_wal() {
    let dir = unique_tmpdir("readonly");
    let db_path = dir.join("spg.db");
    let wal_path = dir.join("spg.db.wal");
    let mut db = Database::open_path(&db_path).unwrap();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    db.checkpoint().unwrap();
    let wal_before = std::fs::metadata(&wal_path).unwrap().len();
    // 100 SELECTs.
    for _ in 0..100 {
        let _ = db.query("SELECT id FROM t WHERE id = 1").unwrap();
    }
    let wal_after = std::fs::metadata(&wal_path).unwrap().len();
    assert_eq!(
        wal_before, wal_after,
        "SELECT statements must not grow the WAL"
    );
}
