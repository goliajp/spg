//! v7.19 P5 — `SELECT unnest(col) FROM t` SRF in projection position.
//!
//! mailrs's redirect_uris pattern (D-pre #2 reverse) uses
//! projection-position unnest; this is the gap docketed under
//! "unnest_projection" in the dropin-acceptance probe.
//! Covers the four shapes the design doc called out + the
//! NULL / empty-array PG semantics.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(qr: QueryResult) -> Vec<Vec<Value<'static>>> {
    match qr {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn basic_unnest_projection_returns_one_row_per_element() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE posts (id INT NOT NULL, tags TEXT[] NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO posts VALUES (1, '{rust,db,wal}')")
        .unwrap();
    let r = e.execute("SELECT unnest(tags) FROM posts").unwrap();
    let rows = rows_of(r);
    assert_eq!(rows.len(), 3, "3 tags → 3 rows");
    assert_eq!(rows[0][0], Value::text("rust"));
    assert_eq!(rows[1][0], Value::text("db"));
    assert_eq!(rows[2][0], Value::text("wal"));
}

#[test]
fn unnest_with_broadcast_id() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE posts (id INT NOT NULL, tags TEXT[] NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO posts VALUES (7, '{a,b}')").unwrap();
    let r = e.execute("SELECT id, unnest(tags) FROM posts").unwrap();
    let rows = rows_of(r);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Int(7));
    assert_eq!(rows[0][1], Value::text("a"));
    assert_eq!(rows[1][0], Value::Int(7));
    assert_eq!(rows[1][1], Value::text("b"));
}

#[test]
fn unnest_int_array() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE m (id INT NOT NULL, ns INT[] NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO m VALUES (1, '{10,20,30}')").unwrap();
    let r = e.execute("SELECT unnest(ns) FROM m").unwrap();
    let rows = rows_of(r);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], Value::Int(10));
    assert_eq!(rows[1][0], Value::Int(20));
    assert_eq!(rows[2][0], Value::Int(30));
}

#[test]
fn unnest_empty_array_returns_zero_rows() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE posts (id INT NOT NULL, tags TEXT[] NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO posts VALUES (1, '{}')").unwrap();
    let r = e.execute("SELECT unnest(tags) FROM posts").unwrap();
    let rows = rows_of(r);
    assert!(rows.is_empty(), "empty array → zero rows (PG semantics)");
}

#[test]
fn unnest_across_multiple_input_rows() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE posts (id INT NOT NULL, tags TEXT[] NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO posts VALUES (1, '{a,b}')").unwrap();
    e.execute("INSERT INTO posts VALUES (2, '{c}')").unwrap();
    e.execute("INSERT INTO posts VALUES (3, '{}')").unwrap();
    let r = e.execute("SELECT id, unnest(tags) FROM posts").unwrap();
    let rows = rows_of(r);
    // 2 + 1 + 0 = 3 rows.
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[0][1], Value::text("a"));
    assert_eq!(rows[1][0], Value::Int(1));
    assert_eq!(rows[1][1], Value::text("b"));
    assert_eq!(rows[2][0], Value::Int(2));
    assert_eq!(rows[2][1], Value::text("c"));
}

#[test]
fn unnest_null_array_yields_no_rows() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE m (id INT NOT NULL, tags TEXT[])")
        .unwrap();
    e.execute("INSERT INTO m VALUES (1, NULL)").unwrap();
    let r = e.execute("SELECT unnest(tags) FROM m").unwrap();
    let rows = rows_of(r);
    assert!(rows.is_empty(), "unnest(NULL) → 0 rows (PG semantics)");
}
