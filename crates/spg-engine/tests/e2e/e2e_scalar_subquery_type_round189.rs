//! v7.39 (read01 round 189) — scalar subquery results keep their
//! type through the literal round-trip.
//!
//! `value_to_literal_expr` materialised BigInt / SmallInt results as
//! bare `Literal::Integer`, which re-types to INT when the value fits
//! i32 — `pg_typeof((SELECT count(*) …))` came back `integer` (PG:
//! `bigint`) on every scalar-subquery shape, a typed-decode breaker
//! for sqlx clients. Now wrapped in an explicit cast. Live-PG18
//! differential 2026-07-18: 5/5 SAME after the fix.

use spg_engine::{Engine, QueryResult};

fn one_text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => format!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn count_in_scalar_subquery_is_bigint() {
    let mut e = Engine::new();
    assert_eq!(
        one_text(
            &mut e,
            "SELECT pg_typeof((SELECT count(*) FROM (VALUES (1)) b(y)))"
        ),
        "bigint"
    );
    // Correlated shape too.
    assert_eq!(
        one_text(
            &mut e,
            "SELECT pg_typeof((SELECT count(*) FROM (VALUES (1)) b(y) WHERE b.y <= a.x)) \
             FROM (VALUES (1)) a(x)"
        ),
        "bigint"
    );
    assert_eq!(
        one_text(
            &mut e,
            "SELECT pg_typeof((SELECT sum(y) FROM (VALUES (1)) b(y)))"
        ),
        "bigint"
    );
}

#[test]
fn smallint_in_scalar_subquery_stays_smallint() {
    let mut e = Engine::new();
    assert_eq!(
        one_text(
            &mut e,
            "SELECT pg_typeof((SELECT x::smallint FROM (VALUES (1)) v(x)))"
        ),
        "smallint"
    );
}

#[test]
fn int_shape_unchanged() {
    let mut e = Engine::new();
    assert_eq!(
        one_text(
            &mut e,
            "SELECT pg_typeof((SELECT x FROM (VALUES (1)) v(x)))"
        ),
        "integer"
    );
    // Value semantics through arithmetic still exact.
    assert_eq!(
        one_text(
            &mut e,
            "SELECT ((SELECT count(*) FROM (VALUES (1),(2)) b(y)) + 1)::text"
        ),
        "3"
    );
}
