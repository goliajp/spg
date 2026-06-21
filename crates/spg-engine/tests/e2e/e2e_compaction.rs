//! v6.7.3 — cold-segment compaction.
//!
//! Engine-level e2e gates for `COMPACT COLD SEGMENTS`. The
//! server-level manifest-atomicity gate
//! (`manifest_swap_is_atomic_under_crash`) lives separately under
//! `crates/spg-server/tests/e2e_compaction_chaos.rs` — it needs
//! the `spg-server` binary + a `db_path` round-trip to exercise
//! the persist+reload path.

use spg_engine::{COMPACTION_TARGET_DEFAULT_BYTES, Engine, QueryResult};
use spg_storage::{IndexKey, Value};

fn populated_users(eng: &mut Engine, n: i64) {
    eng.execute("CREATE TABLE users (id INT NOT NULL, name TEXT)")
        .unwrap();
    eng.execute("CREATE INDEX by_id ON users (id)").unwrap();
    for id in 0..n {
        eng.execute(&format!("INSERT INTO users VALUES ({id}, 'u-{id}')"))
            .unwrap();
    }
}

fn read_rows(res: QueryResult) -> Vec<Vec<Value<'static>>> {
    match res {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values.clone()).collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

/// v6.7.3 L2 ship-gate #1.
/// Two small cold segments produced by back-to-back freezes are
/// merged into a single larger one by `COMPACT COLD SEGMENTS`.
/// Active cold-segment count drops back to 1; every previously
/// frozen PK still resolves through the merged segment.
#[test]
fn compact_merges_small_segments() {
    let mut eng = Engine::new();
    populated_users(&mut eng, 8);
    eng.freeze_oldest_to_cold("users", "by_id", 3).unwrap();
    eng.freeze_oldest_to_cold("users", "by_id", 3).unwrap();
    assert_eq!(eng.catalog().cold_segment_count(), 2);

    let res = eng.execute("COMPACT COLD SEGMENTS").unwrap();
    let rows = read_rows(res);
    assert_eq!(rows.len(), 1, "exactly one (table, index) pair compacted");
    let report = &rows[0];
    assert_eq!(report[0], Value::text("users"));
    assert_eq!(report[1], Value::text("by_id"));
    // sources_merged = 2; merged_rows = 6; deleted_rows_pruned = 0.
    assert_eq!(report[2], Value::BigInt(2));
    assert_eq!(report[4], Value::BigInt(6));
    assert_eq!(report[5], Value::BigInt(0));

    assert_eq!(eng.catalog().cold_segment_count(), 1);
    // Every PK still resolves: the 6 frozen via merged segment, the
    // 2 hot rows in-place.
    for id in 0..8i64 {
        let row = eng
            .catalog()
            .lookup_by_pk("users", "by_id", &IndexKey::Int(id))
            .unwrap_or_else(|| panic!("PK {id} lost after compaction"));
        match &row.values[0] {
            Value::Int(n) => assert_eq!(*n, id as i32),
            other => panic!("PK column not Int: {other:?}"),
        }
    }
}

/// v6.7.3 L2 ship-gate #2.
/// `DELETE`-touched cold rows leave behind shadowed BTree
/// locators; the merge GC's the corresponding payloads. Verify
/// `deleted_rows_pruned` reports the count + the shadowed PKs
/// stay invisible after compaction.
#[test]
fn compaction_drops_deleted_rows() {
    let mut eng = Engine::new();
    populated_users(&mut eng, 6);
    eng.freeze_oldest_to_cold("users", "by_id", 3).unwrap();
    eng.freeze_oldest_to_cold("users", "by_id", 3).unwrap();
    // DELETE crosses through the v5.2.3 shadow_cold_row path for
    // frozen rows. Use the SQL surface so the engine wires the
    // shadow correctly.
    eng.execute("DELETE FROM users WHERE id = 1").unwrap();
    eng.execute("DELETE FROM users WHERE id = 4").unwrap();

    let res = eng.execute("COMPACT COLD SEGMENTS").unwrap();
    let rows = read_rows(res);
    assert_eq!(rows.len(), 1);
    let report = &rows[0];
    assert_eq!(report[2], Value::BigInt(2), "2 sources merged");
    assert_eq!(
        report[4],
        Value::BigInt(4),
        "6 frozen − 2 shadowed = 4 live"
    );
    assert_eq!(report[5], Value::BigInt(2), "2 deleted rows pruned");

    // The deleted PKs do NOT come back through the merged segment.
    for gone in [1i64, 4] {
        let row = eng
            .execute(&format!("SELECT id FROM users WHERE id = {gone}"))
            .unwrap();
        let rows = read_rows(row);
        assert!(
            rows.is_empty(),
            "deleted PK {gone} reappeared after compaction"
        );
    }
    // The non-deleted frozen PKs still resolve.
    for live in [0i64, 2, 3, 5] {
        let row = eng
            .execute(&format!("SELECT id FROM users WHERE id = {live}"))
            .unwrap();
        let rows = read_rows(row);
        assert_eq!(rows.len(), 1, "live PK {live} disappeared");
    }
}

/// Sanity: with only one freeze (one small cold segment) the SQL
/// is a no-op and returns an empty result set.
#[test]
fn compact_is_noop_with_one_segment() {
    let mut eng = Engine::new();
    populated_users(&mut eng, 4);
    eng.freeze_oldest_to_cold("users", "by_id", 2).unwrap();
    let res = eng.execute("COMPACT COLD SEGMENTS").unwrap();
    let rows = read_rows(res);
    assert!(rows.is_empty(), "no merge happened, empty report");
    assert_eq!(eng.catalog().cold_segment_count(), 1);
}

/// Round-trip the catalog snapshot after compaction + reload the
/// merged segment at its baked-in id (mirrors what spg-server's
/// manifest-driven boot does on restart). Every cold row stays
/// resolvable byte-identically across the bounce.
#[test]
fn compact_then_serialize_then_reload_via_load_at() {
    let mut eng = Engine::new();
    populated_users(&mut eng, 6);
    eng.freeze_oldest_to_cold("users", "by_id", 3).unwrap();
    eng.freeze_oldest_to_cold("users", "by_id", 3).unwrap();
    let res = eng
        .compact_cold_segments_with_target(COMPACTION_TARGET_DEFAULT_BYTES)
        .unwrap();
    assert_eq!(res.len(), 1);
    let (_, _, report) = &res[0];
    let merged_id = report.merged_segment_id.unwrap();
    let merged_bytes = report.merged_segment_bytes.clone();

    let snap = eng.catalog().serialize();
    let mut restored_cat = spg_storage::Catalog::deserialize(&snap).expect("deserialize");
    restored_cat
        .load_segment_bytes_at(merged_id, merged_bytes)
        .expect("reload merged");
    for id in 0..6i64 {
        let row = restored_cat
            .lookup_by_pk("users", "by_id", &IndexKey::Int(id))
            .unwrap_or_else(|| panic!("PK {id} lost across roundtrip"));
        match &row.values[0] {
            Value::Int(n) => assert_eq!(*n, id as i32),
            other => panic!("PK column not Int: {other:?}"),
        }
    }
    assert_eq!(restored_cat.cold_segment_count(), 1);
}
