//! v7.38 (read01) — two differential-probe fixes:
//!  * IS [NOT] DISTINCT FROM uses the type's `=` semantics, so
//!    `1 IS NOT DISTINCT FROM 1.0` is true (int and numeric compare equal),
//!    not a representation-exact match.
//!  * DATE - DATE is integer (int4), not bigint.
//! Every expected value / type is from live PG18.4.

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
fn is_distinct_from_uses_equality_semantics() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT (1 IS NOT DISTINCT FROM 1.0)::text"), "true");
    assert_eq!(one(&mut e, "SELECT (1 IS DISTINCT FROM 1.0)::text"), "false");
    assert_eq!(one(&mut e, "SELECT (1 IS DISTINCT FROM 2.0)::text"), "true");
    assert_eq!(one(&mut e, "SELECT (1.5 IS NOT DISTINCT FROM 1.50)::text"), "true");
    assert_eq!(one(&mut e, "SELECT (1::bigint IS NOT DISTINCT FROM 1::int)::text"), "true");
    // Same-type inequality and the NULL-safe arms still hold.
    assert_eq!(one(&mut e, "SELECT ('a' IS DISTINCT FROM 'a')::text"), "false");
    assert_eq!(one(&mut e, "SELECT (NULL::int IS DISTINCT FROM 1)::text"), "true");
    assert_eq!(one(&mut e, "SELECT (NULL IS NOT DISTINCT FROM NULL)::text"), "true");
}

#[test]
fn date_minus_date_is_integer() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT (DATE '2020-03-01' - DATE '2020-01-01')::text"), "60");
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(DATE '2020-03-01' - DATE '2020-01-01')::text"),
        "integer"
    );
}
