//! v7.37.5 β-P4 — `INTERVAL[]` as a stored column type.
//!
//! Mirrors `e2e_interval_column_storage` for the array form. Pins:
//!   * `CREATE TABLE … (col INTERVAL[])` accepted at parser + DDL
//!   * INSERT with `ARRAY[INTERVAL '...', ...]` literal round-trips
//!   * NULL element preserved
//!   * PG byte-equal `'1 day'` ≠ `'24 hours'` survives the array
//!     codec round-trip
//!   * empty array `'{}'` round-trips
//!   * mixed positive / negative / zero spans
//!
//! Wire OID 1187 (`_interval`), catalog tag 35, FILE_VERSION 48+.

use spg_engine::{Engine, QueryResult};
use spg_storage::{DataType, IntervalSpan, Value};

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

fn span(months: i32, days: i32, micros: i64) -> IntervalSpan {
    IntervalSpan {
        months,
        days,
        micros,
    }
}

#[test]
fn create_table_interval_array_column_is_accepted() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, spans INTERVAL[] NOT NULL)")
        .unwrap();
    let r = e.execute("SELECT spans FROM t").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, DataType::IntervalArray);
}

#[test]
fn insert_select_round_trip_interval_array_array_literal() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, spans INTERVAL[] NOT NULL)")
        .unwrap();
    e.execute(
        "INSERT INTO t VALUES \
            (1, ARRAY[INTERVAL '1 day', INTERVAL '24 hours', INTERVAL '2 months'])",
    )
    .unwrap();
    let r = rows(e.execute("SELECT spans FROM t").unwrap());
    assert_eq!(
        r[0][0],
        Value::IntervalArray(vec![
            Some(span(0, 1, 0)),
            // β-P1 made `'24 hours'` distinct from `'1 day'`; β-P4
            // preserves that through the array codec.
            Some(span(0, 0, 86_400_000_000)),
            Some(span(2, 0, 0)),
        ])
    );
}

#[test]
fn nullable_element_round_trips() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, spans INTERVAL[] NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, ARRAY[INTERVAL '1 day', NULL, INTERVAL '5 minutes'])")
        .unwrap();
    let r = rows(e.execute("SELECT spans FROM t").unwrap());
    assert_eq!(
        r[0][0],
        Value::IntervalArray(vec![
            Some(span(0, 1, 0)),
            None,
            Some(span(0, 0, 5 * 60 * 1_000_000)),
        ])
    );
}

#[test]
fn empty_array_round_trips() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, spans INTERVAL[] NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, ARRAY[]::INTERVAL[])")
        .unwrap();
    let r = rows(e.execute("SELECT spans FROM t").unwrap());
    assert_eq!(r[0][0], Value::IntervalArray(vec![]));
}

#[test]
fn negative_and_compound_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, spans INTERVAL[] NOT NULL)")
        .unwrap();
    e.execute(
        "INSERT INTO t VALUES (1, ARRAY[\
            INTERVAL '-1 day', \
            INTERVAL '1 year 6 months', \
            INTERVAL '3 days 12 hours'\
        ])",
    )
    .unwrap();
    let r = rows(e.execute("SELECT spans FROM t").unwrap());
    assert_eq!(
        r[0][0],
        Value::IntervalArray(vec![
            Some(span(0, -1, 0)),
            Some(span(18, 0, 0)),
            Some(span(0, 3, 12 * 3600 * 1_000_000)),
        ])
    );
}

#[test]
fn nullable_column_accepts_null() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, spans INTERVAL[])")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, NULL), (2, ARRAY[INTERVAL '1 day'])")
        .unwrap();
    let r = rows(e.execute("SELECT spans FROM t ORDER BY id").unwrap());
    assert_eq!(r[0][0], Value::Null);
    assert_eq!(
        r[1][0],
        Value::IntervalArray(vec![Some(span(0, 1, 0))])
    );
}
