//! v7.38 (read01) — the array `||` operator treats a NULL array operand as an
//! empty array (PG's array_cat semantics), so `arr || NULL` and `NULL || arr`
//! are the array itself, not NULL. A scalar `text || NULL` still propagates
//! NULL. Every expected value is from live PG18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            spg_storage::Value::Null => "NULL".to_string(),
            other => panic!("{sql}: expected Text/Null, got {other:?}"),
        },
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

#[test]
fn array_concat_with_null_is_the_array() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT (ARRAY['a','b'] || NULL)::text"), "{a,b}");
    assert_eq!(one(&mut e, "SELECT (ARRAY[1,2] || NULL)::text"), "{1,2}");
    assert_eq!(one(&mut e, "SELECT (NULL || ARRAY[1,2])::text"), "{1,2}");
    assert_eq!(one(&mut e, "SELECT (ARRAY[1,2] || NULL::int[])::text"), "{1,2}");
    // Ordinary array concat and element append are unaffected.
    assert_eq!(one(&mut e, "SELECT (ARRAY[1,2] || ARRAY[3])::text"), "{1,2,3}");
    assert_eq!(one(&mut e, "SELECT (ARRAY[1,2] || 3)::text"), "{1,2,3}");
}

#[test]
fn scalar_concat_with_null_still_propagates() {
    // A non-array `||` keeps standard NULL propagation.
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT ('x' || NULL)::text"), "NULL");
    assert_eq!(one(&mut e, "SELECT (NULL || 'y')::text"), "NULL");
    assert_eq!(one(&mut e, "SELECT (5 || NULL)::text"), "NULL");
}
