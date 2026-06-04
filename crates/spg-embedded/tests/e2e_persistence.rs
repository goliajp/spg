//! v7.0 — `spg-embedded` persistence patterns.
//!
//! Strictly speaking the v6.10.3 `Database` ships **in-memory
//! plus byte-slice snapshot round-trip**, not the `spg-server`-
//! style auto-WAL+manifest+CHECKPOINT durability story. But
//! "byte-slice round-trip" + the host's filesystem covers the
//! 80% case: snapshot to bytes, write to a file (anywhere on
//! disk you like), restore from the same bytes on the next
//! process start.
//!
//! What's covered here:
//!   1. `snapshot()` → write `Vec<u8>` to a file → next session
//!      reads the file → `restore(&bytes)` → all rows + indices
//!      survive verbatim.
//!   2. Vector / HNSW state survives the round-trip too — kNN
//!      ranking is preserved post-restore.
//!
//! What's NOT covered (STABILITY § "Out of v6.10" carve-outs):
//!   - Auto-WAL append on every write. The bytes you don't
//!     `snapshot()` are gone on process exit.
//!   - Manifest-driven cold-segment auto-reload. The
//!     `Catalog::cold_segments` registry isn't part of the
//!     `snapshot()` envelope (it's `spg-server`'s manifest's
//!     job in the server topology).
//!   - `Database::open_path(p)` — pending v7.x.

use std::path::PathBuf;

use spg_embedded::Database;
use spg_storage::Value;

fn unique_tmpfile(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("spg-embed-persist-{label}-{nanos}.spg"))
}

#[test]
fn snapshot_to_file_then_restore_round_trips_data() {
    let db_path = unique_tmpfile("basic");
    // "Session 1": populate + snapshot to file.
    {
        let mut db = Database::open_in_memory();
        db.execute("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)")
            .unwrap();
        for (id, name) in [(1, "alice"), (2, "bob"), (3, "carol")] {
            db.execute(&format!("INSERT INTO users VALUES ({id}, '{name}')"))
                .unwrap();
        }
        let bytes = db.snapshot();
        std::fs::write(&db_path, &bytes).unwrap();
    }
    // "Session 2": fresh process, read bytes back, restore.
    let bytes = std::fs::read(&db_path).unwrap();
    let mut db = Database::restore(&bytes).unwrap();
    let rows = db.query("SELECT id, name FROM users WHERE id = 2").unwrap();
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        Value::Int(2) => {}
        other => panic!("expected Int(2), got {other:?}"),
    }
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn snapshot_round_trip_preserves_vector_kNN_ranking() {
    let db_path = unique_tmpfile("vec");
    {
        let mut db = Database::open_in_memory();
        db.execute("CREATE TABLE emb (id INT NOT NULL, v VECTOR(4) NOT NULL)")
            .unwrap();
        db.execute("CREATE INDEX emb_idx ON emb USING hnsw (v)")
            .unwrap();
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
        std::fs::write(&db_path, db.snapshot()).unwrap();
    }
    // Restore + verify kNN order is preserved.
    let bytes = std::fs::read(&db_path).unwrap();
    let mut db = Database::restore(&bytes).unwrap();
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
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn multiple_snapshot_writes_overwrite_cleanly() {
    let db_path = unique_tmpfile("incremental");
    // First snapshot.
    {
        let mut db = Database::open_in_memory();
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        std::fs::write(&db_path, db.snapshot()).unwrap();
    }
    // Second snapshot: load, append, save back.
    {
        let bytes = std::fs::read(&db_path).unwrap();
        let mut db = Database::restore(&bytes).unwrap();
        db.execute("INSERT INTO t VALUES (2)").unwrap();
        db.execute("INSERT INTO t VALUES (3)").unwrap();
        std::fs::write(&db_path, db.snapshot()).unwrap();
    }
    // Verify final state.
    let bytes = std::fs::read(&db_path).unwrap();
    let mut db = Database::restore(&bytes).unwrap();
    let got = db.query("SELECT count(*) FROM t").unwrap();
    match &got[0][0] {
        Value::BigInt(3) => {}
        other => panic!("expected BigInt(3), got {other:?}"),
    }
    let _ = std::fs::remove_file(&db_path);
}
