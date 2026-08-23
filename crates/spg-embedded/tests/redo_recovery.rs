// The single `set_var` below is unsafe in edition 2024; this standalone
// single-test binary has no concurrent env reader, so it is sound. The
// crate-wide `deny(unsafe_code)` doesn't fit a test that must set an env.
#![allow(unsafe_code)]
//! v7.34 (crash-recovery P0 #2) — end-to-end crash recovery via row-level
//! redo (`SPG_WAL_ROW_REDO`). A standalone single-test binary so it owns
//! the process environment: `std::env::set_var` is UB in Rust 2024 if
//! another thread reads the env concurrently, and the merged-e2e binary
//! runs its tests in parallel — here there is exactly one test.
//!
//! Proves the row-level redo path replaces the O(records × catalog_rows)
//! statement-replay (the superlinear hang root-caused on the mailrs P0):
//! DML is logged as physical 0x13 records and DDL as SQL (hybrid log);
//! a simulated crash (skip Drop → no checkpoint) recovers by replaying
//! the redo + DDL, NOT by re-executing the DML.

use spg_embedded::Database;
use spg_storage::Value;

#[test]
fn row_redo_survives_simulated_crash() {
    // SAFETY: standalone single-test binary — no other thread reads the
    // environment concurrently with this set.
    unsafe {
        std::env::set_var("SPG_WAL_ROW_REDO", "1");
    }

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir()
        .join("spg-tests")
        .join(format!("spg-redo-recovery-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("spg.db");

    // Session 1: DDL (logged as SQL) + DML (logged as 0x13 redo), then a
    // simulated crash — mem::forget skips Drop's checkpoint, so the WAL
    // alone (no snapshot of these writes) must rebuild the state.
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')")
            .unwrap();
        db.execute("UPDATE t SET v = 'B' WHERE id = 2").unwrap();
        db.execute("DELETE FROM t WHERE id = 1").unwrap();
        std::mem::forget(db);
    }
    Database::force_unlock(&db_path).unwrap();

    // Session 2: boot replay applies the redo (DML) + re-runs the SQL
    // (DDL) → the committed state is reconstructed exactly.
    let mut db = Database::open_path(&db_path).unwrap();
    let rows = db.query("SELECT id, v FROM t ORDER BY id").unwrap();
    assert_eq!(
        rows.len(),
        2,
        "expected rows id=2,3 after replay, got {rows:?}"
    );
    assert!(
        matches!(rows[0][0], Value::Int(2)),
        "row0 id: {:?}",
        rows[0][0]
    );
    assert!(
        matches!(&rows[0][1], Value::Text(s) if s == "B"),
        "row0 v: {:?}",
        rows[0][1]
    );
    assert!(
        matches!(rows[1][0], Value::Int(3)),
        "row1 id: {:?}",
        rows[1][0]
    );
    assert!(
        matches!(&rows[1][1], Value::Text(s) if s == "c"),
        "row1 v: {:?}",
        rows[1][1]
    );

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
