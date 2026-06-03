//! v7.9.10 — ON CONFLICT with composite (multi-column) target.
//! Covers mailrs' CalDAV / CardDAV `(uid, calendar_id)` shape.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        let r = eng
            .execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
        assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql:?}");
    }
    eng
}

fn select(eng: &mut Engine, sql: &str) -> Vec<Vec<Value>> {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn composite_target_do_nothing_skips_full_match() {
    let mut eng = engine_with(&[
        "CREATE TABLE cal (uid INT NOT NULL, calendar_id INT NOT NULL, payload TEXT)",
        "INSERT INTO cal VALUES (1, 100, 'v1'), (2, 100, 'v2')",
    ]);
    // (1, 100) already present → skip; (1, 200) is new → insert.
    eng.execute(
        "INSERT INTO cal VALUES (1, 100, 'dup'), (1, 200, 'new') \
         ON CONFLICT (uid, calendar_id) DO NOTHING",
    )
    .unwrap();
    let rows = select(&mut eng, "SELECT uid, calendar_id FROM cal");
    assert_eq!(rows.len(), 3);
}

#[test]
fn composite_target_do_update_writes_post_state() {
    let mut eng = engine_with(&[
        "CREATE TABLE cal (uid INT NOT NULL, calendar_id INT NOT NULL, payload TEXT NOT NULL, etag TEXT NOT NULL)",
        "INSERT INTO cal VALUES (1, 100, 'v1', 'e1')",
    ]);
    eng.execute(
        "INSERT INTO cal VALUES (1, 100, 'v2', 'e2') \
         ON CONFLICT (uid, calendar_id) DO UPDATE SET payload = EXCLUDED.payload, etag = EXCLUDED.etag",
    )
    .unwrap();
    let rows = select(&mut eng, "SELECT payload, etag FROM cal");
    assert_eq!(rows[0][0], Value::Text("v2".into()));
    assert_eq!(rows[0][1], Value::Text("e2".into()));
}

#[test]
fn composite_target_partial_match_inserts() {
    // (1, 100) is in the table. (1, 200) shares only uid — not
    // a conflict on the composite key.
    let mut eng = engine_with(&[
        "CREATE TABLE cal (uid INT NOT NULL, calendar_id INT NOT NULL, payload TEXT)",
        "INSERT INTO cal VALUES (1, 100, 'a')",
    ]);
    eng.execute(
        "INSERT INTO cal VALUES (1, 200, 'b') \
         ON CONFLICT (uid, calendar_id) DO NOTHING",
    )
    .unwrap();
    let rows = select(&mut eng, "SELECT calendar_id FROM cal");
    assert_eq!(rows.len(), 2);
}

#[test]
fn composite_target_within_batch_dedup() {
    let mut eng = engine_with(&[
        "CREATE TABLE cal (uid INT NOT NULL, calendar_id INT NOT NULL)",
    ]);
    // Two rows with the same (uid, calendar_id) in the same batch.
    eng.execute(
        "INSERT INTO cal VALUES (1, 100), (1, 100), (1, 101) \
         ON CONFLICT (uid, calendar_id) DO NOTHING",
    )
    .unwrap();
    let rows = select(&mut eng, "SELECT uid FROM cal");
    assert_eq!(rows.len(), 2);
}

#[test]
fn composite_target_unknown_column_is_rejected() {
    let mut eng = engine_with(&[
        "CREATE TABLE cal (uid INT NOT NULL, calendar_id INT NOT NULL)",
    ]);
    let r = eng.execute(
        "INSERT INTO cal VALUES (1, 100) ON CONFLICT (uid, ghost) DO NOTHING",
    );
    assert!(r.is_err());
}
