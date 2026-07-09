//! v7.38 (read01) — ARRAY constructor element-type unification across the
//! numeric ladder, matching PG. `ARRAY[1, 2.5]` is numeric[] (not text[]),
//! `ARRAY[1, 2.5::float8]` is double precision[]; each numeric element keeps
//! its own scale. Every expected value / type is from live PG18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => panic!("{sql}: expected Text, got {other:?}"),
        },
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

#[test]
fn int_numeric_mix_is_numeric_array() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_typeof(ARRAY[1, 2.5, 3])::text"), "numeric[]");
    assert_eq!(one(&mut e, "SELECT (ARRAY[1, 2.5, 3])::text"), "{1,2.5,3}");
    assert_eq!(one(&mut e, "SELECT pg_typeof(ARRAY[1::bigint, 2.5])::text"), "numeric[]");
    // Each element keeps its own scale.
    assert_eq!(one(&mut e, "SELECT (ARRAY[1.50, 2])::text"), "{1.50,2}");
    assert_eq!(one(&mut e, "SELECT (ARRAY[1.5, 2.55, 3])::text"), "{1.5,2.55,3}");
}

#[test]
fn float_in_the_mix_is_double_array() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_typeof(ARRAY[1, 2.5::float8])::text"), "double precision[]");
    assert_eq!(one(&mut e, "SELECT (ARRAY[1, 2.5::float8])::text"), "{1,2.5}");
    assert_eq!(one(&mut e, "SELECT pg_typeof(ARRAY[1::float8, 2::float8])::text"), "double precision[]");
}

#[test]
fn all_integer_stays_integer_array() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_typeof(ARRAY[1, 2, 3])::text"), "integer[]");
    assert_eq!(one(&mut e, "SELECT pg_typeof(ARRAY[1::bigint, 2])::text"), "bigint[]");
}

#[test]
fn subscript_of_numeric_array_is_numeric() {
    // The element is numeric, so `[i] / 2` is numeric division, not integer.
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_typeof((ARRAY[1, 2.5, 3])[2])::text"), "numeric");
    assert_eq!(one(&mut e, "SELECT ((ARRAY[1, 2.5, 3])[2] / 2)::text"), "1.25000000000000000000");
}
