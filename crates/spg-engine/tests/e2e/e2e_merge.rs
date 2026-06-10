//! v7.17.0 Phase 3.5 → P0-42 — SQL:2003 / PG 15+ MERGE statement.
//!
//! Status: Phase 3.5 carved this out; v7.17.0 Phase 3.P0-42 lands
//! the real implementation. SPG accepts:
//!   * `MERGE INTO target [alias] USING source [alias] ON expr`
//!   * `WHEN MATCHED [AND expr] THEN { UPDATE SET … | DELETE | DO NOTHING }`
//!   * `WHEN NOT MATCHED [AND expr] THEN { INSERT (cols) VALUES (vals) | DO NOTHING }`
//!
//! v7.17 limitations (carved out as separate follow-ups):
//!   * Source must be a catalog table (no subquery source yet)
//!   * No RETURNING clause
//!   * No BY SOURCE / BY TARGET (PG 17+ extensions)
//!   * No row triggers / WAL bookkeeping inside the MERGE write path
//!   * No cardinality enforcement (PG-canonical: a target row
//!     covered twice raises an error; SPG silently applies the
//!     last firing action)
//!
//! INSERT … ON CONFLICT … DO UPDATE still works as the
//! upsert-shaped alternative.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE target (id INT NOT NULL PRIMARY KEY, val INT NOT NULL)")
        .unwrap();
    e.execute("CREATE TABLE source (id INT NOT NULL, val INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO target VALUES (1, 100), (2, 200)")
        .unwrap();
    e.execute("INSERT INTO source VALUES (1, 150), (3, 300)")
        .unwrap();
}

#[test]
fn merge_upsert_updates_matched_and_inserts_not_matched() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute(
        "MERGE INTO target t USING source s ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET val = s.val \
         WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val)",
    )
    .unwrap();
    let r = rows(e.execute("SELECT id, val FROM target ORDER BY id").unwrap());
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][1], Value::Int(150), "id=1 updated from 100 → 150");
    assert_eq!(r[1][1], Value::Int(200), "id=2 unchanged");
    assert_eq!(r[2][1], Value::Int(300), "id=3 inserted");
}

#[test]
fn merge_matched_delete_removes_matched_rows() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute(
        "MERGE INTO target t USING source s ON t.id = s.id \
         WHEN MATCHED THEN DELETE",
    )
    .unwrap();
    let r = rows(e.execute("SELECT id FROM target ORDER BY id").unwrap());
    // id=1 deleted (matched); id=2 stays (no match in source).
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(2));
}

#[test]
fn merge_with_when_matched_and_condition_filters() {
    // Use AND condition to update only when source val > 100.
    let mut e = Engine::new();
    setup(&mut e);
    e.execute(
        "MERGE INTO target t USING source s ON t.id = s.id \
         WHEN MATCHED AND s.val > 100 THEN UPDATE SET val = s.val",
    )
    .unwrap();
    let r = rows(e.execute("SELECT id, val FROM target ORDER BY id").unwrap());
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][1], Value::Int(150), "id=1 update fired (150 > 100)");
    assert_eq!(r[1][1], Value::Int(200), "id=2 unchanged (no source match)");
}

#[test]
fn merge_when_not_matched_only() {
    // Only the NOT MATCHED clause is provided — matched source
    // rows fall through without changes.
    let mut e = Engine::new();
    setup(&mut e);
    e.execute(
        "MERGE INTO target t USING source s ON t.id = s.id \
         WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val)",
    )
    .unwrap();
    let r = rows(e.execute("SELECT id, val FROM target ORDER BY id").unwrap());
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][1], Value::Int(100), "id=1 NOT updated");
    assert_eq!(r[2][1], Value::Int(300), "id=3 inserted");
}

#[test]
fn merge_do_nothing_explicit() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute(
        "MERGE INTO target t USING source s ON t.id = s.id \
         WHEN MATCHED THEN DO NOTHING \
         WHEN NOT MATCHED THEN DO NOTHING",
    )
    .unwrap();
    let r = rows(e.execute("SELECT id, val FROM target ORDER BY id").unwrap());
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][1], Value::Int(100));
    assert_eq!(r[1][1], Value::Int(200));
}

#[test]
fn insert_on_conflict_do_update_workaround() {
    // PG's ON CONFLICT DO UPDATE covers the dominant MERGE
    // use case: upsert. Verified end-to-end here so the
    // customer-facing workaround doc has a known-good shape.
    let mut e = Engine::new();
    setup(&mut e);
    e.execute(
        "INSERT INTO target (id, val) VALUES (1, 150), (3, 300) \
         ON CONFLICT (id) DO UPDATE SET val = EXCLUDED.val",
    )
    .unwrap();
    let r = rows(e.execute("SELECT id, val FROM target ORDER BY id").unwrap());
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][1], Value::Int(150), "id=1 updated");
    assert_eq!(r[1][1], Value::Int(200), "id=2 unchanged");
    assert_eq!(r[2][1], Value::Int(300), "id=3 inserted");
}

#[test]
fn insert_on_conflict_do_nothing_alternative() {
    // The "INSERT-only-if-not-exists" MERGE shape maps to
    // ON CONFLICT DO NOTHING.
    let mut e = Engine::new();
    setup(&mut e);
    e.execute(
        "INSERT INTO target (id, val) VALUES (1, 999), (4, 400) \
         ON CONFLICT (id) DO NOTHING",
    )
    .unwrap();
    let r = rows(e.execute("SELECT id, val FROM target ORDER BY id").unwrap());
    assert_eq!(r.len(), 3, "id=4 inserted; id=1 preserved");
    assert_eq!(r[0][1], Value::Int(100), "id=1 NOT updated");
}
