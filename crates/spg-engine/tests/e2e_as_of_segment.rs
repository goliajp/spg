//! v6.10.2 — Cold-tier time travel via `AS OF SEGMENT '<id>'`.
//!
//! Operator-facing time travel for cold-tier forensics:
//! `SELECT … FROM <tbl> AS OF SEGMENT '<seg_id>'` scans only
//! the named cold segment, ignoring the hot tier + any other
//! segments. Scope is intentionally narrow (projection + WHERE +
//! LIMIT) — JOINs / aggregates / ORDER BY require restoring the
//! segment into a regular table first (STABILITY § "Out of
//! v6.10").

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn as_of_segment_returns_only_frozen_rows() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX by_id ON t (id)").unwrap();
    for id in 0..6i64 {
        e.execute(&format!("INSERT INTO t VALUES ({id}, 'r-{id}')"))
            .unwrap();
    }
    // Freeze the first 3 rows into seg 0; keep the rest hot.
    e.freeze_oldest_to_cold("t", "by_id", 3).unwrap();
    // AS OF SEGMENT '0' returns the 3 frozen rows only — no hot rows.
    let rows = rows_of(
        e.execute("SELECT id FROM t AS OF SEGMENT '0'").unwrap(),
    );
    assert_eq!(rows.len(), 3);
    let mut ids: Vec<i64> = rows
        .into_iter()
        .map(|r| match r.into_iter().next().unwrap() {
            Value::Int(n) => n as i64,
            other => panic!("expected Int, got {other:?}"),
        })
        .collect();
    ids.sort();
    assert_eq!(ids, vec![0, 1, 2]);
}

#[test]
fn as_of_segment_supports_where_filter() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX by_id ON t (id)").unwrap();
    for id in 0..6i64 {
        e.execute(&format!("INSERT INTO t VALUES ({id}, 'r-{id}')"))
            .unwrap();
    }
    e.freeze_oldest_to_cold("t", "by_id", 3).unwrap();
    let rows = rows_of(
        e.execute("SELECT id FROM t AS OF SEGMENT '0' WHERE id = 1")
            .unwrap(),
    );
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        Value::Int(1) => {}
        other => panic!("expected Int(1), got {other:?}"),
    }
}

#[test]
fn as_of_segment_supports_limit() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX by_id ON t (id)").unwrap();
    for id in 0..6i64 {
        e.execute(&format!("INSERT INTO t VALUES ({id}, 'r-{id}')"))
            .unwrap();
    }
    e.freeze_oldest_to_cold("t", "by_id", 5).unwrap();
    let rows = rows_of(
        e.execute("SELECT id FROM t AS OF SEGMENT '0' LIMIT 2")
            .unwrap(),
    );
    assert_eq!(rows.len(), 2);
}

#[test]
fn as_of_segment_with_unknown_id_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let r = e.execute("SELECT id FROM t AS OF SEGMENT '42'");
    assert!(r.is_err(), "unknown segment id must error");
}

#[test]
fn as_of_segment_rejects_join() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("CREATE TABLE u (id INT NOT NULL)").unwrap();
    e.execute("CREATE INDEX by_id ON t (id)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    e.freeze_oldest_to_cold("t", "by_id", 1).unwrap();
    let r = e.execute(
        "SELECT t.id FROM t AS OF SEGMENT '0' JOIN u ON t.id = u.id",
    );
    assert!(r.is_err(), "AS OF SEGMENT + JOIN must surface STABILITY carve-out error");
}

#[test]
fn as_of_segment_parses_and_display_round_trips() {
    use spg_sql::ast::Statement;
    use spg_sql::parser::parse_statement;
    let sql = "SELECT id FROM t AS OF SEGMENT '5' WHERE id = 1";
    let stmt = parse_statement(sql).unwrap();
    let Statement::Select(ref s) = stmt else {
        panic!("expected Select");
    };
    assert_eq!(s.from.as_ref().unwrap().primary.as_of_segment, Some(5));
}
