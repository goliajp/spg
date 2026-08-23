//! v7.37.15 (Epic W) — END-TO-END crash-recovery durability proof for the
//! gate-on (`SPG_MVCC_INPLACE`) in-place DELETE / UPDATE tombstone, driven
//! through the REAL embedded `Database` WAL-write + reopen-replay path.
//!
//! The storage-level unit tests
//! (`spg_storage::tests::redo_tombstone_survives_replay_hidden_from_snapshot`
//! / `redo_update_tombstone_plus_insert_survives_replay`) already prove the
//! codec + `Catalog::apply_redo` layer in isolation, where the INSERT and
//! the tombstone ride ONE `drain_redo` run. This binary proves the harder
//! FULL path: an autocommit DELETE / UPDATE persists a `0x13` ROW_REDO
//! record (`encode_v6_row_redo` via `wal_after_ok`), the process "crashes"
//! before the next checkpoint (`mem::forget` skips Drop's checkpoint), and
//! a fresh reopen replays the WAL. Because each autocommit statement is a
//! SEPARATE `0x13` record, the INSERT tombstone-target RowIds and the
//! DELETE/UPDATE tombstone RowIds resolve across DISTINCT `apply_redo`
//! runs — the case the single-run unit tests can't exercise. (The gate-on
//! RowId allocation is deterministic — `Table::next_rowid` starts at 1 and
//! increments per row — so the reopen's INSERT-redo run re-materialises the
//! exact ids the DELETE-redo tombstone names.)
//!
//! Enablement mechanism: the gate lives on the engine
//! (`Engine::set_mvcc_inplace`). `Database` already surfaces the engine via
//! the public `engine_mut()` escape hatch, so a test flips the gate on
//! WITHOUT any production env wiring (kept intentionally test-only until the
//! write path is proven durable against PG18 differential tests). No new
//! plumbing was added.
//!
//! Durability claim proven here: the MVCC-VISIBLE result of a gate-on
//! DELETE/UPDATE survives a real crash+replay UNCHANGED — the tombstoned
//! row does NOT resurrect. Each test also asserts the recovered state is
//! byte-faithful to the pre-crash in-process gate-on state (captured before
//! `mem::forget`), which is a stronger claim than a hardcoded expectation
//! and stays valid if the orthogonal count(*) gap below is later closed.
//!
//! ORTHOGONAL GAP (NOT a durability defect — reproduces in-process without
//! any crash): `count(*)` over a gate-on table counts tombstoned rows. The
//! single-table aggregate full-scan in
//! `crates/spg-engine/src/select.rs::run_single_table_aggregate` (the
//! `for i in 0..table.row_count()` loop, ~line 3185) reads `table.rows()`
//! with NO visibility filter, unlike the plain-scan path that uses
//! `table.scan_visible(&snap)` (`select.rs` ~line 108). So a gate-on
//! `count(*)` returns the PHYSICAL row count, tombstones included. Because
//! the gate is test-only / OFF in production this is not a customer-facing
//! bug today, but it is a real gate-on completeness gap for whoever finishes
//! Epic W. These tests therefore assert count(*) is REPRODUCED faithfully by
//! replay (same value before/after crash), not that it equals the
//! MVCC-correct survivor count — the visible-scan assertions carry the
//! actual durability proof.
//!
//! These tests do NOT touch `SPG_WAL_ROW_REDO`; row-level redo is the
//! production default-ON (v7.37.8), so a fresh `Database::open_path` already
//! arms `0x13` capture. Nothing here mutates the process environment, so
//! the default parallel test execution is sound.

use spg_embedded::Database;
use spg_storage::Value;

/// A unique temp db path so parallel tests never collide.
fn fresh_db_path(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir()
        .join("spg-tests")
        .join(format!("spg-tomb-recovery-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("spg.db");
    (dir, db_path)
}

/// Extract an integer cell as i64 regardless of the concrete integer width
/// (count(*) is BigInt, plain INT columns are Int).
fn as_i64(v: &Value<'_>) -> i64 {
    match v {
        Value::SmallInt(n) => i64::from(*n),
        Value::Int(n) => i64::from(*n),
        Value::BigInt(n) => *n,
        other => panic!("expected an integer cell, got {other:?}"),
    }
}

/// Read the sole `count(*)` scalar.
fn scalar_count(db: &mut Database, sql: &str) -> i64 {
    let rows = db.query(sql).unwrap();
    assert_eq!(rows.len(), 1, "count query returns one row: {rows:?}");
    assert_eq!(rows[0].len(), 1, "count query returns one column: {rows:?}");
    as_i64(&rows[0][0])
}

/// Read a single INT column, ORDER BY-projected, as a Vec<i64>.
fn ordered_ids(db: &mut Database, sql: &str) -> Vec<i64> {
    db.query(sql)
        .unwrap()
        .iter()
        .map(|r| as_i64(&r[0]))
        .collect()
}

/// GATE-ON DELETE. A tombstoning (in-place) DELETE must survive a real
/// crash+recover cycle through the actual WAL-write + reopen-replay path:
/// the deleted row must NOT resurrect from the MVCC-visible set.
#[test]
fn gate_on_delete_tombstone_survives_real_crash_recovery() {
    let (dir, db_path) = fresh_db_path("del-on");

    // Session 1: gate-on writes, capture the in-process gate-on state, then
    // a simulated crash (skip Drop → no checkpoint) so recovery must replay
    // the WAL alone.
    let (pre_visible, pre_count);
    {
        let mut db = Database::open_path(&db_path).unwrap();
        // Enablement: flip the in-place MVCC write gate through the public
        // engine escape hatch. Test-only; no production env is read.
        db.engine_mut().set_mvcc_inplace(true);
        assert!(
            db.engine().mvcc_inplace(),
            "gate must be armed before the DELETE"
        );

        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
        // Gate-on: this tombstones id=2 (stamps xmax, keeps the row) and
        // persists a 0x13 Tombstone redo record.
        db.execute("DELETE FROM t WHERE id = 2").unwrap();

        // Pre-crash in-process gate-on state (the fidelity baseline).
        pre_visible = ordered_ids(&mut db, "SELECT id FROM t ORDER BY id");
        pre_count = scalar_count(&mut db, "SELECT count(*) FROM t");
        assert_eq!(
            pre_visible,
            vec![1, 3],
            "gate-on in-process: id=2 is hidden from the visible set"
        );

        std::mem::forget(db); // simulate crash before the next checkpoint
    }
    Database::force_unlock(&db_path).unwrap();

    // Session 2: reopen → WAL replay. The tombstone's RowId (captured at
    // DELETE time in session 1) must resolve against the RowIds the INSERT
    // redo re-materialises in a SEPARATE apply_redo run.
    let mut db = Database::open_path(&db_path).unwrap();
    let ids = ordered_ids(&mut db, "SELECT id FROM t ORDER BY id");
    // THE durability proof: the tombstoned row did NOT resurrect through the
    // real WAL replay path.
    assert_eq!(
        ids,
        vec![1, 3],
        "tombstoned row id=2 must NOT resurrect through real WAL replay"
    );
    // Fidelity: recovery reproduces the exact gate-on state (visible set +
    // count, count included so a future closure of the aggregate-visibility
    // gap keeps this test honest).
    assert_eq!(
        ids, pre_visible,
        "visible set must survive replay unchanged"
    );
    assert_eq!(
        scalar_count(&mut db, "SELECT count(*) FROM t"),
        pre_count,
        "count(*) reproduced faithfully by replay (see aggregate-visibility gap in module docs)"
    );

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// GATE-OFF control (production default). The SAME DELETE, run physically
/// (legacy `delete_rows` → `Delete` redo), must recover to the SAME visible
/// result {1, 3} — proving the gate-on path is observably equivalent to the
/// gate-off path across a crash. (Gate-off count(*) IS 2 because the physical
/// delete removes the row, which is exactly why the gate-on count(*)=3 is a
/// gate-on aggregate-visibility gap, not a durability defect.)
#[test]
fn gate_off_delete_matches_gate_on_after_recovery() {
    let (dir, db_path) = fresh_db_path("del-off");

    {
        let mut db = Database::open_path(&db_path).unwrap();
        // v7.39 — the flip made in-place MVCC the production default;
        // this control group forces the LEGACY physical-delete path
        // explicitly.
        db.engine_mut().set_mvcc_inplace(false);
        assert!(!db.engine().mvcc_inplace(), "control group is gate-off");
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
        db.execute("DELETE FROM t WHERE id = 2").unwrap();
        std::mem::forget(db);
    }
    Database::force_unlock(&db_path).unwrap();

    let mut db = Database::open_path(&db_path).unwrap();
    let ids = ordered_ids(&mut db, "SELECT id FROM t ORDER BY id");
    assert_eq!(
        ids,
        vec![1, 3],
        "gate-off physical DELETE recovers to the same visible result as gate-on"
    );
    // Gate-off physically removes the row, so count(*) is the correct 2.
    assert_eq!(scalar_count(&mut db, "SELECT count(*) FROM t"), 2);

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// GATE-ON UPDATE. A gate-on UPDATE supersedes a row by tombstoning the old
/// version and appending the new one. Across a real crash+recover, the old
/// value must stay hidden and the new value must be visible — the visible
/// set unchanged in cardinality.
#[test]
fn gate_on_update_supersede_survives_real_crash_recovery() {
    let (dir, db_path) = fresh_db_path("upd-on");

    let (pre_v2, pre_ids, pre_count);
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.engine_mut().set_mvcc_inplace(true);
        db.execute("CREATE TABLE t (id INT, v TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')")
            .unwrap();
        // Gate-on UPDATE: tombstone old (id=2,'b'), append new (id=2,'B').
        db.execute("UPDATE t SET v = 'B' WHERE id = 2").unwrap();

        pre_v2 = db
            .query("SELECT v FROM t WHERE id = 2")
            .unwrap()
            .iter()
            .map(|r| match &r[0] {
                Value::Text(s) => s.to_string(),
                other => panic!("v must be text, got {other:?}"),
            })
            .collect::<Vec<_>>();
        pre_ids = ordered_ids(&mut db, "SELECT id FROM t ORDER BY id");
        pre_count = scalar_count(&mut db, "SELECT count(*) FROM t");
        assert_eq!(
            pre_v2,
            vec!["B".to_string()],
            "in-process: only new value visible"
        );
        assert_eq!(pre_ids, vec![1, 2, 3], "in-process: three visible rows");

        std::mem::forget(db);
    }
    Database::force_unlock(&db_path).unwrap();

    let mut db = Database::open_path(&db_path).unwrap();
    // The updated row's new value must be the ONLY id=2 visible after replay.
    let v2: Vec<String> = db
        .query("SELECT v FROM t WHERE id = 2")
        .unwrap()
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.to_string(),
            other => panic!("v must be text, got {other:?}"),
        })
        .collect();
    assert_eq!(
        v2,
        vec!["B".to_string()],
        "recovered id=2 must show ONLY the NEW value 'B' (old 'b' tombstoned)"
    );
    let ids = ordered_ids(&mut db, "SELECT id FROM t ORDER BY id");
    assert_eq!(
        ids,
        vec![1, 2, 3],
        "UPDATE keeps three visible rows (old version hidden, new shown)"
    );
    // Fidelity across the crash.
    assert_eq!(v2, pre_v2);
    assert_eq!(ids, pre_ids);
    assert_eq!(
        scalar_count(&mut db, "SELECT count(*) FROM t"),
        pre_count,
        "count(*) reproduced faithfully by replay (aggregate-visibility gap: old+new both physical)"
    );

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
