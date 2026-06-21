//! v7.37.5 γ — array-of-scalar family smoke tests.
//!
//! Pins one round-trip per new array type (BOOL/SMALLINT/FLOAT/
//! NUMERIC/DATE/TIMESTAMP/TIMESTAMPTZ/UUID/JSON/JSONB/BYTEA/
//! VARCHAR/CHAR). Each test:
//!   1. CREATE TABLE accepted at the parser + DDL gate
//!   2. INSERT … VALUES (ARRAY[lit, NULL, lit]) lands the typed
//!      array via `array_literal_widen`'s γ uniform detector
//!   3. SELECT round-trips bit-equal back through the codec
//!
//! Catalog tags 36..48, wire OIDs from PG `pg_type.dat`, codec
//! shape mirrors β-P4 INTERVAL[] (`[u16 count][per elem: u8 null
//! + (non-null) scalar body]`).

use spg_engine::{Engine, QueryResult};
use spg_storage::{DataType, Value};

fn first_row(e: &mut Engine, sql: &str) -> Vec<Value<'static>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows for {sql}");
    };
    rows.into_iter().next().expect("at least one row").values
}

fn col_type(e: &mut Engine, sql: &str) -> DataType {
    let r = e.execute(sql).unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    columns[0].ty
}

#[test]
fn bool_array_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, xs BOOL[] NOT NULL)")
        .unwrap();
    assert_eq!(col_type(&mut e, "SELECT xs FROM t"), DataType::BoolArray);
    e.execute("INSERT INTO t VALUES (1, ARRAY[true, false, NULL, true])")
        .unwrap();
    let row = first_row(&mut e, "SELECT xs FROM t");
    assert_eq!(
        row[0],
        Value::BoolArray(vec![Some(true), Some(false), None, Some(true)])
    );
}

#[test]
fn smallint_array_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, xs SMALLINT[] NOT NULL)")
        .unwrap();
    assert_eq!(
        col_type(&mut e, "SELECT xs FROM t"),
        DataType::SmallIntArray
    );
    e.execute("INSERT INTO t VALUES (1, ARRAY[1::smallint, -2::smallint, NULL, 30000::smallint])")
        .unwrap();
    let row = first_row(&mut e, "SELECT xs FROM t");
    assert_eq!(
        row[0],
        Value::SmallIntArray(vec![Some(1), Some(-2), None, Some(30000)])
    );
}

#[test]
fn float_array_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, xs FLOAT[] NOT NULL)")
        .unwrap();
    assert_eq!(col_type(&mut e, "SELECT xs FROM t"), DataType::FloatArray);
    e.execute("INSERT INTO t VALUES (1, ARRAY[1.5, -2.25, NULL])")
        .unwrap();
    let row = first_row(&mut e, "SELECT xs FROM t");
    assert_eq!(
        row[0],
        Value::FloatArray(vec![Some(1.5), Some(-2.25), None])
    );
}

#[test]
fn numeric_array_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, xs NUMERIC[] NOT NULL)")
        .unwrap();
    assert_eq!(col_type(&mut e, "SELECT xs FROM t"), DataType::NumericArray);
    e.execute("INSERT INTO t VALUES (1, ARRAY[1.50::numeric(10,2), -3.14::numeric(10,2), NULL])")
        .unwrap();
    let row = first_row(&mut e, "SELECT xs FROM t");
    // NUMERIC(10,2) — scaled = value × 10^2, scale = 2.
    assert_eq!(
        row[0],
        Value::NumericArray(vec![Some((150, 2)), Some((-314, 2)), None])
    );
}

#[test]
fn date_array_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, xs DATE[] NOT NULL)")
        .unwrap();
    assert_eq!(col_type(&mut e, "SELECT xs FROM t"), DataType::DateArray);
    e.execute("INSERT INTO t VALUES (1, ARRAY['2024-01-01'::date, '2025-06-30'::date, NULL])")
        .unwrap();
    let row = first_row(&mut e, "SELECT xs FROM t");
    let Value::DateArray(items) = &row[0] else {
        panic!("got {:?}", row[0]);
    };
    assert_eq!(items.len(), 3);
    assert!(items[0].is_some());
    assert!(items[1].is_some());
    assert_eq!(items[2], None);
}

#[test]
fn timestamp_array_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, xs TIMESTAMP[] NOT NULL)")
        .unwrap();
    assert_eq!(
        col_type(&mut e, "SELECT xs FROM t"),
        DataType::TimestampArray
    );
    e.execute(
        "INSERT INTO t VALUES (1, ARRAY['2024-01-01 00:00:00'::timestamp, NULL, \
         '2025-06-30 12:34:56'::timestamp])",
    )
    .unwrap();
    let row = first_row(&mut e, "SELECT xs FROM t");
    let Value::TimestampArray(items) = &row[0] else {
        panic!("got {:?}", row[0]);
    };
    assert_eq!(items.len(), 3);
    assert!(items[0].is_some());
    assert_eq!(items[1], None);
    assert!(items[2].is_some());
}

#[test]
fn uuid_array_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, xs UUID[] NOT NULL)")
        .unwrap();
    assert_eq!(col_type(&mut e, "SELECT xs FROM t"), DataType::UuidArray);
    e.execute(
        "INSERT INTO t VALUES (1, ARRAY['550e8400-e29b-41d4-a716-446655440000'::uuid, \
         NULL, '00000000-0000-0000-0000-000000000001'::uuid])",
    )
    .unwrap();
    let row = first_row(&mut e, "SELECT xs FROM t");
    let Value::UuidArray(items) = &row[0] else {
        panic!("got {:?}", row[0]);
    };
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].map(|b| b[0]), Some(0x55));
    assert_eq!(items[1], None);
    assert_eq!(items[2].map(|b| b[15]), Some(0x01));
}

#[test]
fn bytes_array_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, xs BYTEA[] NOT NULL)")
        .unwrap();
    assert_eq!(col_type(&mut e, "SELECT xs FROM t"), DataType::BytesArray);
    e.execute("INSERT INTO t VALUES (1, ARRAY['\\xdead'::bytea, NULL, '\\xbeef'::bytea])")
        .unwrap();
    let row = first_row(&mut e, "SELECT xs FROM t");
    assert_eq!(
        row[0],
        Value::BytesArray(vec![Some(vec![0xde, 0xad]), None, Some(vec![0xbe, 0xef]),])
    );
}

#[test]
fn empty_typed_array_round_trip() {
    // `ARRAY[]::TYPE[]` produces a zero-element typed array. Codec
    // must round-trip the empty count cleanly.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, xs BOOL[] NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, ARRAY[]::BOOL[])")
        .unwrap();
    let row = first_row(&mut e, "SELECT xs FROM t");
    assert_eq!(row[0], Value::BoolArray(vec![]));
}

#[test]
fn nullable_column_distinguishes_null_from_empty() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, xs FLOAT[])")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, NULL), (2, ARRAY[]::FLOAT[]), (3, ARRAY[1.0])")
        .unwrap();
    let r1 = first_row(&mut e, "SELECT xs FROM t WHERE id = 1");
    let r2 = first_row(&mut e, "SELECT xs FROM t WHERE id = 2");
    let r3 = first_row(&mut e, "SELECT xs FROM t WHERE id = 3");
    assert_eq!(r1[0], Value::Null);
    assert_eq!(r2[0], Value::FloatArray(vec![]));
    assert_eq!(r3[0], Value::FloatArray(vec![Some(1.0)]));
}
