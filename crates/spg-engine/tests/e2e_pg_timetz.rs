//! v7.17.0 Phase 3.P0-34 — PG `TIME WITH TIME ZONE` / `TIMETZ`.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/datatype-datetime.html
//!
//! Surface:
//!   * `CREATE TABLE … (col TIMETZ)` — DDL accept.
//!   * `INSERT … VALUES ('14:30:45+00')` — text → TimeTz(i64, i32).
//!   * `INSERT … VALUES ('14:30:45.123456-05:30')` — sub-second
//!     precision + non-whole-hour offset (e.g. India IST = +05:30,
//!     Newfoundland NST = -03:30).
//!   * `SELECT col FROM t` — Value::TimeTz round-trip via canonical
//!     `HH:MM:SS[.ffffff]±HH:MM` text.
//!   * Catalog snapshot survival (encode + decode tag 27).
//!   * NULL handling.
//!
//! Invariants pinned:
//!   * Storage: i64 micros since 00:00:00 in the **local** wall
//!     clock (NOT shifted to UTC — PG preserves the offset on
//!     output) + i32 seconds offset from UTC (positive = east of
//!     UTC, range ±50400 = ±14 h).
//!   * Display: `HH:MM:SS` zero-padded + optional `.ffffff` (trim
//!     trailing zeros, matches PG `timetz_out`) + signed offset
//!     `±HH` for whole-hour offsets, `±HH:MM` for sub-hour.
//!   * Out-of-range offset → hard SQL error.

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
fn ddl_accepts_timetz_keyword() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, started TIMETZ)")
        .unwrap();
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let schema = cat.get("t").unwrap().schema();
    assert!(matches!(schema.columns[1].ty, DataType::TimeTz));
}

#[test]
fn insert_utc_offset_round_trips() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, started TIMETZ)",
        "INSERT INTO t VALUES (1, '14:30:45+00')",
    ]);
    let rows = select(&mut eng, "SELECT started FROM t");
    let Value::TimeTz { us, offset_secs } = &rows[0][0] else {
        panic!("expected Value::TimeTz, got {:?}", rows[0][0]);
    };
    assert_eq!(*us, 52_245_000_000);
    assert_eq!(*offset_secs, 0);
}

#[test]
fn insert_positive_whole_hour_offset() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, started TIMETZ)",
        "INSERT INTO t VALUES (1, '09:00:00+09')",
    ]);
    let rows = select(&mut eng, "SELECT started FROM t");
    let Value::TimeTz { us, offset_secs } = &rows[0][0] else { panic!() };
    assert_eq!(*us, 32_400_000_000);
    assert_eq!(*offset_secs, 9 * 3600);
}

#[test]
fn insert_negative_sub_hour_offset_newfoundland() {
    // Newfoundland Standard Time is UTC-03:30 — a real-world
    // sub-hour offset. Pinning it catches sign + minutes parsing.
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, started TIMETZ)",
        "INSERT INTO t VALUES (1, '08:30:00-03:30')",
    ]);
    let rows = select(&mut eng, "SELECT started FROM t");
    let Value::TimeTz { us, offset_secs } = &rows[0][0] else { panic!() };
    assert_eq!(*us, 30_600_000_000);
    assert_eq!(*offset_secs, -(3 * 3600 + 30 * 60));
}

#[test]
fn insert_microseconds_with_india_offset() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, started TIMETZ)",
        "INSERT INTO t VALUES (1, '14:30:45.123456+05:30')",
    ]);
    let rows = select(&mut eng, "SELECT started FROM t");
    let Value::TimeTz { us, offset_secs } = &rows[0][0] else { panic!() };
    assert_eq!(*us, 52_245_123_456);
    assert_eq!(*offset_secs, 5 * 3600 + 30 * 60);
}

#[test]
fn timetz_column_survives_catalog_round_trip() {
    let mut eng = engine_with(&[
        "CREATE TABLE meetings (id INT NOT NULL, started TIMETZ)",
        "INSERT INTO meetings VALUES (1, '09:00:00+00'), (2, '14:30:00-05:00')",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let mut eng2 = Engine::restore(cat);
    let rows = select(&mut eng2, "SELECT id, started FROM meetings ORDER BY id");
    assert_eq!(rows.len(), 2);
    let Value::TimeTz { us: a_us, offset_secs: a_off } = &rows[0][1] else { panic!() };
    let Value::TimeTz { us: b_us, offset_secs: b_off } = &rows[1][1] else { panic!() };
    assert_eq!(*a_us, 32_400_000_000);
    assert_eq!(*a_off, 0);
    assert_eq!(*b_us, 52_200_000_000);
    assert_eq!(*b_off, -5 * 3600);
}

#[test]
fn timetz_null_column() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, started TIMETZ)",
        "INSERT INTO t VALUES (1, NULL)",
    ]);
    let rows = select(&mut eng, "SELECT started FROM t");
    assert!(matches!(rows[0][0], Value::Null));
}

#[test]
fn timetz_malformed_input_is_error() {
    let mut eng = engine_with(&["CREATE TABLE t (id INT NOT NULL, started TIMETZ)"]);
    let r = eng.execute("INSERT INTO t VALUES (1, 'not a time')");
    assert!(r.is_err(), "garbage TIMETZ literal must error");
}

#[test]
fn timetz_missing_offset_is_error() {
    // PG accepts a TIME literal in a TIMETZ column by assuming
    // the session TZ — SPG has no session TZ wired through here
    // so we surface as a hard error. App must spell the offset.
    let mut eng = engine_with(&["CREATE TABLE t (id INT NOT NULL, started TIMETZ)"]);
    let r = eng.execute("INSERT INTO t VALUES (1, '14:30:45')");
    assert!(r.is_err(), "TIMETZ literal without an offset must error in SPG");
}

#[test]
fn timetz_offset_out_of_range_is_error() {
    let mut eng = engine_with(&["CREATE TABLE t (id INT NOT NULL, started TIMETZ)"]);
    let r = eng.execute("INSERT INTO t VALUES (1, '14:30:45+15')");
    assert!(r.is_err(), "offset > +14h must error");
}
