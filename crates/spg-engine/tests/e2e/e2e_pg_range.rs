//! v7.17.0 Phase 3.P0-38 — PG range types.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/rangetypes.html
//!
//! Surface (all 6 builtin range types):
//!   * `int4range` (OID 3904)  — int range
//!   * `int8range` (OID 3926)  — bigint range
//!   * `numrange`  (OID 3906)  — numeric range
//!   * `tsrange`   (OID 3908)  — timestamp range
//!   * `tstzrange` (OID 3910)  — timestamptz range
//!   * `daterange` (OID 3912)  — date range
//!
//! All six share a single `Value::Range { kind, lower, upper,
//! lower_inc, upper_inc, empty }` shape; the `kind` tag pins the
//! element type (RangeKind) so encode/decode and Display can
//! route off one switch.
//!
//! Invariants pinned:
//!   * Inclusivity surfaces in canonical text via `[` / `(` for
//!     lower and `]` / `)` for upper.
//!   * Unbounded sides render as empty between bracket and comma:
//!     `'[1,)'` = `>= 1`, `'(,10]'` = `<= 10`.
//!   * `'empty'` literal is a valid input — round-trips verbatim.
//!   * NULL preserved.
//!   * Malformed input → hard SQL error.
//!
//! v7.17.0 ships parse + display + storage + DDL accept; range
//! OPERATORS (`@>`, `&&`, `<<`, `<@`, `*`, `+`) land in a
//! follow-up phase — they need their own planner integration.

use spg_engine::{Engine, QueryResult};
use spg_storage::{DataType, RangeKind, Value};

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
fn ddl_accepts_all_six_range_keywords() {
    let mut eng = Engine::new();
    eng.execute(
        "CREATE TABLE t (
            id INT NOT NULL,
            i4 INT4RANGE,
            i8 INT8RANGE,
            num NUMRANGE,
            ts TSRANGE,
            tstz TSTZRANGE,
            d DATERANGE
        )",
    )
    .unwrap();
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let schema = cat.get("t").unwrap().schema();
    assert!(matches!(
        schema.columns[1].ty,
        DataType::Range(RangeKind::Int4)
    ));
    assert!(matches!(
        schema.columns[2].ty,
        DataType::Range(RangeKind::Int8)
    ));
    assert!(matches!(
        schema.columns[3].ty,
        DataType::Range(RangeKind::Num)
    ));
    assert!(matches!(
        schema.columns[4].ty,
        DataType::Range(RangeKind::Ts)
    ));
    assert!(matches!(
        schema.columns[5].ty,
        DataType::Range(RangeKind::TsTz)
    ));
    assert!(matches!(
        schema.columns[6].ty,
        DataType::Range(RangeKind::Date)
    ));
}

#[test]
fn insert_int4range_half_open_round_trips() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, r INT4RANGE)",
        "INSERT INTO t VALUES (1, '[1,10)')",
    ]);
    let rows = select(&mut eng, "SELECT r FROM t");
    let Value::Range {
        kind,
        lower,
        upper,
        lower_inc,
        upper_inc,
        empty,
    } = &rows[0][0]
    else {
        panic!("expected Range, got {:?}", rows[0][0]);
    };
    assert!(matches!(kind, RangeKind::Int4));
    assert_eq!(lower.as_deref(), Some(&Value::Int(1)));
    assert_eq!(upper.as_deref(), Some(&Value::Int(10)));
    assert!(*lower_inc);
    assert!(!*upper_inc);
    assert!(!*empty);
}

#[test]
fn insert_int4range_closed_round_trips() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, r INT4RANGE)",
        "INSERT INTO t VALUES (1, '[1,10]')",
    ]);
    let rows = select(&mut eng, "SELECT r FROM t");
    // PG canonicalizes a discrete int4range to the `[)` form, so
    // '[1,10]' round-trips as '[1,11)' (read01 U26). Live PG 18.4:
    // `'[1,10]'::int4range` = `[1,11)`.
    let Value::Range {
        lower,
        upper,
        lower_inc,
        upper_inc,
        ..
    } = &rows[0][0]
    else {
        panic!()
    };
    assert_eq!(lower.as_deref(), Some(&Value::Int(1)));
    assert_eq!(upper.as_deref(), Some(&Value::Int(11)));
    assert!(*lower_inc);
    assert!(!*upper_inc);
}

#[test]
fn insert_int4range_unbounded_lower() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, r INT4RANGE)",
        "INSERT INTO t VALUES (1, '(,10]')",
    ]);
    let rows = select(&mut eng, "SELECT r FROM t");
    let Value::Range { lower, upper, upper_inc, .. } = &rows[0][0] else {
        panic!()
    };
    // Discrete canonicalization: '(,10]' → '(,11)' (read01 U26). The
    // inclusive upper bumps to an exclusive 11; live PG 18.4 agrees.
    assert!(lower.is_none());
    assert_eq!(upper.as_deref(), Some(&Value::Int(11)));
    assert!(!*upper_inc);
}

#[test]
fn insert_int4range_unbounded_upper() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, r INT4RANGE)",
        "INSERT INTO t VALUES (1, '[1,)')",
    ]);
    let rows = select(&mut eng, "SELECT r FROM t");
    let Value::Range { lower, upper, .. } = &rows[0][0] else {
        panic!()
    };
    assert_eq!(lower.as_deref(), Some(&Value::Int(1)));
    assert!(upper.is_none());
}

#[test]
fn insert_empty_range() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, r INT4RANGE)",
        "INSERT INTO t VALUES (1, 'empty')",
    ]);
    let rows = select(&mut eng, "SELECT r FROM t");
    let Value::Range { empty, .. } = &rows[0][0] else {
        panic!()
    };
    assert!(*empty);
}

#[test]
fn insert_int8range_round_trips() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, r INT8RANGE)",
        "INSERT INTO t VALUES (1, '[9999999999,99999999999)')",
    ]);
    let rows = select(&mut eng, "SELECT r FROM t");
    let Value::Range {
        kind, lower, upper, ..
    } = &rows[0][0]
    else {
        panic!()
    };
    assert!(matches!(kind, RangeKind::Int8));
    assert_eq!(lower.as_deref(), Some(&Value::BigInt(9_999_999_999)));
    assert_eq!(upper.as_deref(), Some(&Value::BigInt(99_999_999_999)));
}

#[test]
fn insert_daterange_round_trips() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, r DATERANGE)",
        "INSERT INTO t VALUES (1, '[2025-01-01,2025-12-31)')",
    ]);
    let rows = select(&mut eng, "SELECT r FROM t");
    let Value::Range {
        kind, lower, upper, ..
    } = &rows[0][0]
    else {
        panic!()
    };
    assert!(matches!(kind, RangeKind::Date));
    // Date stored as i32 days since 1970-01-01.
    assert!(matches!(lower.as_deref(), Some(Value::Date(_))));
    assert!(matches!(upper.as_deref(), Some(Value::Date(_))));
}

#[test]
fn range_null_column() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, r INT4RANGE)",
        "INSERT INTO t VALUES (1, NULL)",
    ]);
    let rows = select(&mut eng, "SELECT r FROM t");
    assert!(matches!(rows[0][0], Value::Null));
}

#[test]
fn range_column_survives_catalog_round_trip() {
    let mut eng = engine_with(&[
        "CREATE TABLE bookings (id INT NOT NULL, period TSRANGE)",
        "INSERT INTO bookings VALUES \
         (1, '[2025-06-01 09:00:00,2025-06-01 10:00:00)'), \
         (2, 'empty')",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let mut eng2 = Engine::restore(cat);
    let rows = select(&mut eng2, "SELECT id, period FROM bookings ORDER BY id");
    assert_eq!(rows.len(), 2);
    let Value::Range { kind, empty, .. } = &rows[1][1] else {
        panic!()
    };
    assert!(matches!(kind, RangeKind::Ts));
    assert!(*empty);
}

#[test]
fn malformed_range_input_is_error() {
    let mut eng = engine_with(&["CREATE TABLE t (id INT NOT NULL, r INT4RANGE)"]);
    let r = eng.execute("INSERT INTO t VALUES (1, '[1,)abc')");
    assert!(r.is_err(), "garbage range literal must error");
}

#[test]
fn range_display_round_trips_canonical_text() {
    // SELECT must return the canonical form regardless of how the
    // literal was spelled at INSERT (PG normalises whitespace).
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, r INT4RANGE)",
        "INSERT INTO t VALUES (1, '[1,10)')",
    ]);
    // Use cast to text to validate the display form.
    let r = eng.execute("SELECT r::text FROM t").unwrap();
    match r {
        QueryResult::Rows { rows, .. } => {
            let Value::Text(s) = &rows[0].values[0] else {
                panic!()
            };
            assert_eq!(s, "[1,10)");
        }
        _ => panic!(),
    }
}

#[test]
fn timestamp_range_bounds_are_quoted() {
    // PG's range_out double-quotes a bound containing whitespace, so a
    // tsrange prints its timestamp bounds in quotes (verified vs live
    // PG18.4). A multirange quotes each member's bounds the same way.
    // Space-free bounds (numrange / int4range) stay unquoted.
    let mut eng = Engine::new();
    let got = select(
        &mut eng,
        "SELECT tsrange('2020-01-01 10:00','2020-02-01 12:00')::text",
    );
    assert_eq!(
        got[0][0],
        Value::text("[\"2020-01-01 10:00:00\",\"2020-02-01 12:00:00\")")
    );
    let got = select(
        &mut eng,
        "SELECT tsmultirange(tsrange('2020-01-01 10:00','2020-02-01 12:00'))::text",
    );
    assert_eq!(
        got[0][0],
        Value::text("{[\"2020-01-01 10:00:00\",\"2020-02-01 12:00:00\")}")
    );
    // Control: numeric bounds have no whitespace → no quoting.
    let got = select(&mut eng, "SELECT numrange(1.5, 3.5)::text");
    assert_eq!(got[0][0], Value::text("[1.5,3.5)"));
}
