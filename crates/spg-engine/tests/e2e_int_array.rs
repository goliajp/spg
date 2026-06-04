//! v7.11.13-14 — INT[] / BIGINT[] surface (Epic 3 of v7.11).
//! Storage codec, ARRAY[…] literal widening, scalar ops
//! (subscript / ANY / ALL / array_length / array_position /
//! unnest / ||), and ::INT[] / ::BIGINT[] cast.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn ok(eng: &mut Engine, sql: &str) -> QueryResult {
    eng.execute(sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"))
}

fn select_value(eng: &mut Engine, sql: &str) -> Value {
    match ok(eng, sql) {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .next()
            .map(|mut r| r.values.remove(0))
            .expect("at least one row"),
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn select_rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Value>> {
    match ok(eng, sql) {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn int_array_create_insert_select_roundtrip() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (xs INT[] NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (ARRAY[1, 2, 3])");
    let v = select_value(&mut eng, "SELECT xs FROM t");
    assert!(matches!(v, Value::IntArray(ref a) if a.as_slice() == [Some(1), Some(2), Some(3)]));
}

#[test]
fn int_array_with_nulls() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (xs INT[] NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (ARRAY[1, NULL, 3])");
    let v = select_value(&mut eng, "SELECT xs FROM t");
    assert!(matches!(v, Value::IntArray(ref a) if a.as_slice() == [Some(1), None, Some(3)]));
}

#[test]
fn int_array_subscript_returns_int() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (xs INT[] NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (ARRAY[10, 20, 30])");
    let v = select_value(&mut eng, "SELECT xs[2] FROM t");
    assert!(matches!(v, Value::Int(20)));
}

#[test]
fn int_array_subscript_out_of_range_returns_null() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (xs INT[] NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (ARRAY[10, 20])");
    let v = select_value(&mut eng, "SELECT xs[5] FROM t");
    assert!(matches!(v, Value::Null));
}

#[test]
fn int_array_any() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (xs INT[] NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (ARRAY[1, 2, 3])");
    let v = select_value(&mut eng, "SELECT 2 = ANY(xs) FROM t");
    assert!(matches!(v, Value::Bool(true)));
}

#[test]
fn int_array_all() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (xs INT[] NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (ARRAY[10, 20, 30])");
    let v = select_value(&mut eng, "SELECT 5 < ALL(xs) FROM t");
    assert!(matches!(v, Value::Bool(true)));
}

#[test]
fn int_array_length() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (xs INT[] NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (ARRAY[1, 2, 3, 4, 5])");
    let v = select_value(&mut eng, "SELECT array_length(xs, 1) FROM t");
    assert!(matches!(v, Value::Int(5)));
}

#[test]
fn int_array_position() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (xs INT[] NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (ARRAY[7, 11, 13])");
    let v = select_value(&mut eng, "SELECT array_position(xs, 11) FROM t");
    assert!(matches!(v, Value::Int(2)));
    let v = select_value(&mut eng, "SELECT array_position(xs, 999) FROM t");
    assert!(matches!(v, Value::Null));
}

#[test]
fn int_array_unnest_emits_ints() {
    let mut eng = Engine::new();
    let rows = select_rows(
        &mut eng,
        "SELECT * FROM unnest(ARRAY[10, 20, 30]) AS u",
    );
    assert_eq!(rows.len(), 3);
    assert!(matches!(rows[0][0], Value::Int(10)));
    assert!(matches!(rows[1][0], Value::Int(20)));
    assert!(matches!(rows[2][0], Value::Int(30)));
}

#[test]
fn int_array_concat_array_to_array() {
    let mut eng = Engine::new();
    let v = select_value(
        &mut eng,
        "SELECT ARRAY[1, 2] || ARRAY[3, 4]",
    );
    assert!(
        matches!(v, Value::IntArray(ref a) if a.as_slice() == [Some(1), Some(2), Some(3), Some(4)])
    );
}

#[test]
fn int_array_append_scalar() {
    let mut eng = Engine::new();
    let v = select_value(&mut eng, "SELECT ARRAY[1, 2] || 99");
    assert!(matches!(v, Value::IntArray(ref a) if a.as_slice() == [Some(1), Some(2), Some(99)]));
}

#[test]
fn bigint_array_create_insert_select() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (xs BIGINT[] NOT NULL)");
    ok(
        &mut eng,
        "INSERT INTO t VALUES (ARRAY[9223372036854775807, 1, -1])",
    );
    let v = select_value(&mut eng, "SELECT xs FROM t");
    assert!(matches!(
        v,
        Value::BigIntArray(ref a)
            if a.as_slice() == [Some(9_223_372_036_854_775_807_i64), Some(1), Some(-1)]
    ));
}

#[test]
fn bigint_array_subscript_returns_bigint() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (xs BIGINT[] NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (ARRAY[1000000000000, 2])");
    let v = select_value(&mut eng, "SELECT xs[1] FROM t");
    assert!(matches!(v, Value::BigInt(1_000_000_000_000_i64)));
}

#[test]
fn int_array_cast_from_text() {
    let mut eng = Engine::new();
    let v = select_value(&mut eng, "SELECT '{1,2,3}'::INT[]");
    assert!(matches!(v, Value::IntArray(ref a) if a.as_slice() == [Some(1), Some(2), Some(3)]));
}

#[test]
fn bigint_array_cast_widening_from_int_array() {
    let mut eng = Engine::new();
    let v = select_value(&mut eng, "SELECT (ARRAY[1, 2, 3])::BIGINT[]");
    assert!(
        matches!(v, Value::BigIntArray(ref a) if a.as_slice() == [Some(1_i64), Some(2), Some(3)])
    );
}
