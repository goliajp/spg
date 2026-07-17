//! v7.39 (read01 round 110) — `real` (float4) → `numeric` cast.
//!
//! `float8 → numeric` had a coercion arm but `real → numeric` had none, so the
//! value stayed a REAL and the column/type check rejected it
//! (`expected NUMERIC, got REAL`). Added the arm, formatting the f32 via its
//! own shortest round-trip decimal (so `0.1::real::numeric` is `0.1`, not the
//! f64-widened `0.10000000149…`). Locked byte-identical against live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn real_to_numeric_constrained() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT (1.5::real)::numeric(10,4)::text"),
        "1.5000"
    );
    assert_eq!(
        text(&mut e, "SELECT (2.5::float4)::numeric(6,2)::text"),
        "2.50"
    );
    assert_eq!(
        text(&mut e, "SELECT (100.25::real)::numeric(8,2)::text"),
        "100.25"
    );
    assert_eq!(
        text(&mut e, "SELECT (-3.5::real)::numeric(5,1)::text"),
        "-3.5"
    );
    // Rounds to the declared scale.
    assert_eq!(
        text(&mut e, "SELECT (123.456::real)::numeric(5,2)::text"),
        "123.46"
    );
}

#[test]
fn real_to_unconstrained_numeric_keeps_shortest_decimal() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT (1.5::real)::numeric::text"), "1.5");
    assert_eq!(text(&mut e, "SELECT (0.5::real)::numeric::text"), "0.5");
    // f32 shortest decimal, not the f64-widened form.
    assert_eq!(text(&mut e, "SELECT (0.1::real)::numeric::text"), "0.1");
}

#[test]
fn float8_to_numeric_unchanged() {
    // Regression guard for the sibling arm.
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT (1.5::float8)::numeric(10,4)::text"),
        "1.5000"
    );
    assert_eq!(text(&mut e, "SELECT (0.1::float8)::numeric::text"), "0.1");
}
