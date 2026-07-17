//! v7.39 (read01 round 98) — `round(numeric, N)` with a NEGATIVE N.
//!
//! `round(1234.5678, -2)` → `1200` in both engines, but SPG returned it as
//! `double precision` while PG keeps `numeric`: the exact-rounding branch only
//! covered `0..=38`, so a negative target scale (round to tens / hundreds / …)
//! fell through to the f64 path and changed the result type. An integer
//! argument (the `round(numeric, int)` overload via an implicit cast) hit the
//! same f64 path. Both now round the mantissa exactly and stay numeric.
//! Values AND types locked against live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn negative_ndigits_stays_numeric() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT round(1234.5678, -2)::text"), "1200");
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(round(1234.5678, -2))::text"),
        "numeric"
    );
    // Integer input is the round(numeric,int) overload → numeric, not float.
    assert_eq!(text(&mut e, "SELECT round(12345, -2)::text"), "12300");
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(round(12345, -2))::text"),
        "numeric"
    );
}

#[test]
fn negative_ndigits_values_match_pg() {
    let mut e = Engine::new();
    // Half-away-from-zero at the rounding place, both signs, carry past it.
    assert_eq!(text(&mut e, "SELECT round(1250.0, -2)::text"), "1300");
    assert_eq!(text(&mut e, "SELECT round(-1250.0, -2)::text"), "-1300");
    assert_eq!(text(&mut e, "SELECT round(999.9, -3)::text"), "1000");
    assert_eq!(text(&mut e, "SELECT round(1234.5678, -1)::text"), "1230");
    assert_eq!(text(&mut e, "SELECT round(49, -2)::text"), "0");
    assert_eq!(text(&mut e, "SELECT round(50, -2)::text"), "100");
    assert_eq!(text(&mut e, "SELECT round(0, -2)::text"), "0");
}

#[test]
fn positive_and_float_paths_unchanged() {
    // Regression guard: positive scale stays numeric + exact; a genuine float
    // 1-arg round stays double precision with banker's rounding.
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT round(1234.5678, 2)::text"), "1234.57");
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(round(1.5, 2))::text"),
        "numeric"
    );
    assert_eq!(
        text(&mut e, "SELECT round(1.255::numeric, 2)::text"),
        "1.26"
    );
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(round(2.5::float8))::text"),
        "double precision"
    );
    assert_eq!(text(&mut e, "SELECT round(2.5::float8)::text"), "2");
}
