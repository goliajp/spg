//! v7.17.0 Phase 3.5 — MERGE statement (SQL:2003 / PG 15+).
//!
//! Status: SPG's parser doesn't model the MERGE statement.
//! Implementing MERGE end-to-end (target/source resolution,
//! WHEN MATCHED / NOT MATCHED branches, action dispatch,
//! row-level RETURNING) is a multi-day refactor carved out
//! for v7.18.
//!
//! Customer workaround for the dominant upsert shape:
//! PG's `INSERT … ON CONFLICT … DO UPDATE` (which SPG already
//! supports) covers ~ 90% of the customer MERGE use cases.

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
    e.execute("INSERT INTO target VALUES (1, 100), (2, 200)").unwrap();
    e.execute("INSERT INTO source VALUES (1, 150), (3, 300)")
        .unwrap();
}

#[test]
fn merge_statement_is_documented_gap() {
    let mut e = Engine::new();
    setup(&mut e);
    let r = e.execute(
        "MERGE INTO target t USING source s ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET val = s.val \
         WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val)",
    );
    assert!(
        r.is_err(),
        "MERGE is documented gap in v7.17; expected parse error"
    );
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
    let r = rows(
        e.execute("SELECT id, val FROM target ORDER BY id").unwrap(),
    );
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
    let r = rows(
        e.execute("SELECT id, val FROM target ORDER BY id").unwrap(),
    );
    assert_eq!(r.len(), 3, "id=4 inserted; id=1 preserved");
    assert_eq!(r[0][1], Value::Int(100), "id=1 NOT updated");
}
