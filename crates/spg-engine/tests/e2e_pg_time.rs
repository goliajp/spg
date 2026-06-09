//! v7.17.0 Phase 3.P0-32 — PG `TIME` type (i64 microseconds since
//! midnight, OID 1083).
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/datatype-datetime.html
//!
//! Surface:
//!   * `CREATE TABLE … (col TIME)` — DDL accept.
//!   * `INSERT … VALUES ('14:30:45')` — text → Time(i64 us).
//!   * `INSERT … VALUES ('14:30:45.123456')` — sub-second precision.
//!   * `SELECT col FROM t` — Value::Time round-trip.
//!   * Catalog snapshot survival (encode + decode tag 25).
//!   * NULL handling.
//!
//! Invariants pinned:
//!   * Display: zero-padded `HH:MM:SS` when fractional is zero,
//!     `HH:MM:SS.ffffff` otherwise (PG canonical).
//!   * Storage: i64 microseconds since 00:00:00 (range
//!     0..86_400_000_000; PG also allows 24:00:00 but SPG clamps
//!     at first encoding/parse for symmetry — out-of-range parse
//!     is a hard SQL error).

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

fn select(eng: &mut Engine, sql: &str) -> Vec<Vec<Value>> {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn ddl_accepts_time_keyword() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, alarm TIME)")
        .unwrap();
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let schema = cat.get("t").unwrap().schema();
    assert!(matches!(schema.columns[1].ty, DataType::Time));
}

#[test]
fn insert_and_select_time_round_trips() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, alarm TIME)",
        "INSERT INTO t VALUES (1, '14:30:45')",
    ]);
    let rows = select(&mut eng, "SELECT alarm FROM t");
    assert_eq!(rows.len(), 1);
    let Value::Time(us) = &rows[0][0] else {
        panic!("expected Value::Time, got {:?}", rows[0][0]);
    };
    // 14:30:45 = (14*3600 + 30*60 + 45) seconds = 52245 sec
    assert_eq!(*us, 52_245_000_000);
}

#[test]
fn insert_with_microseconds() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, alarm TIME)",
        "INSERT INTO t VALUES (1, '14:30:45.123456')",
    ]);
    let rows = select(&mut eng, "SELECT alarm FROM t");
    let Value::Time(us) = &rows[0][0] else { panic!() };
    assert_eq!(*us, 52_245_123_456);
}

#[test]
fn time_zero_midnight_round_trips() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, alarm TIME)",
        "INSERT INTO t VALUES (1, '00:00:00')",
    ]);
    let rows = select(&mut eng, "SELECT alarm FROM t");
    let Value::Time(us) = &rows[0][0] else { panic!() };
    assert_eq!(*us, 0);
}

#[test]
fn time_end_of_day_round_trips() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, alarm TIME)",
        "INSERT INTO t VALUES (1, '23:59:59.999999')",
    ]);
    let rows = select(&mut eng, "SELECT alarm FROM t");
    let Value::Time(us) = &rows[0][0] else { panic!() };
    assert_eq!(*us, 86_399_999_999);
}

#[test]
fn time_column_survives_catalog_round_trip() {
    let mut eng = engine_with(&[
        "CREATE TABLE schedule (id INT NOT NULL, alarm TIME)",
        "INSERT INTO schedule VALUES (1, '08:30:00'), (2, '17:45:30')",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let mut eng2 = Engine::restore(cat);
    let rows = select(&mut eng2, "SELECT id, alarm FROM schedule ORDER BY id");
    assert_eq!(rows.len(), 2);
    let Value::Time(a) = &rows[0][1] else { panic!() };
    let Value::Time(b) = &rows[1][1] else { panic!() };
    assert_eq!(*a, 30_600_000_000); // 08:30:00
    assert_eq!(*b, 63_930_000_000); // 17:45:30
}

#[test]
fn time_null_column() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, alarm TIME)",
        "INSERT INTO t VALUES (1, NULL)",
    ]);
    let rows = select(&mut eng, "SELECT alarm FROM t");
    assert!(matches!(rows[0][0], Value::Null));
}

#[test]
fn time_malformed_input_is_error() {
    let mut eng = engine_with(&["CREATE TABLE t (id INT NOT NULL, alarm TIME)"]);
    let r = eng.execute("INSERT INTO t VALUES (1, 'not a time')");
    assert!(r.is_err(), "garbage TIME literal must error, not silently store");
}

#[test]
fn time_out_of_range_hour_is_error() {
    let mut eng = engine_with(&["CREATE TABLE t (id INT NOT NULL, alarm TIME)"]);
    let r = eng.execute("INSERT INTO t VALUES (1, '25:00:00')");
    assert!(r.is_err(), "hour > 23 must error");
}
