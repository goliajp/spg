//! v7.9.2 — TIMESTAMPTZ keyword alias. Storage shape identical
//! to TIMESTAMP (i64 microseconds UTC); only the schema-side
//! type tag (and downstream PG-wire OID) differ.

use spg_engine::{Engine, QueryResult};
use spg_storage::{DataType, Value};

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

fn select(eng: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn ddl_accepts_timestamptz_keyword() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, ts TIMESTAMPTZ)")
        .unwrap();
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    assert!(matches!(
        cat.get("t").unwrap().schema().columns[1].ty,
        DataType::Timestamptz
    ));
}

#[test]
fn insert_and_select_timestamptz_round_trip() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, sent_at TIMESTAMPTZ NOT NULL)",
        "INSERT INTO t VALUES (1, '2026-06-04 12:34:56')",
    ]);
    let rows = select(&mut eng, "SELECT sent_at FROM t");
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Timestamp(_)));
}

#[test]
fn timestamp_and_timestamptz_are_interchangeable_for_storage() {
    // SPG's storage layer treats them as same i64 — DDL flavour
    // is purely a wire-OID label.
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, a TIMESTAMP, b TIMESTAMPTZ)",
        "INSERT INTO t VALUES (1, '2026-06-04 12:00:00', '2026-06-04 12:00:00')",
    ]);
    let rows = select(&mut eng, "SELECT a, b FROM t");
    assert_eq!(rows[0][0], rows[0][1]);
}

#[test]
fn schema_with_only_timestamptz_columns_works_end_to_end() {
    // Full DDL → INSERT → SELECT covering a real-world shape
    // (audit log) where every time column is TIMESTAMPTZ.
    let mut eng = engine_with(&[
        "CREATE TABLE audit_log (
            id INT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL,
            updated_at TIMESTAMPTZ NOT NULL,
            expires_at TIMESTAMPTZ
        )",
        "INSERT INTO audit_log VALUES (1, '2026-06-04 09:00:00', '2026-06-04 09:00:00', NULL)",
        "INSERT INTO audit_log VALUES (2, '2026-06-04 09:01:00', '2026-06-04 09:05:00', '2026-06-30 00:00:00')",
    ]);
    let rows = select(&mut eng, "SELECT id, expires_at FROM audit_log");
    assert_eq!(rows.len(), 2);
    assert!(matches!(rows[0][1], Value::Null));
    assert!(matches!(rows[1][1], Value::Timestamp(_)));
}

#[test]
fn timestamptz_round_trips_through_catalog_snapshot() {
    let mut eng = engine_with(&[
        "CREATE TABLE messages (id INT NOT NULL, sent_at TIMESTAMPTZ NOT NULL)",
        "INSERT INTO messages VALUES (1, '2026-06-04 09:00:00')",
        "INSERT INTO messages VALUES (2, '2026-06-04 10:00:00')",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let mut eng2 = Engine::restore(cat);
    let rows = select(&mut eng2, "SELECT sent_at FROM messages");
    assert_eq!(rows.len(), 2);
}
