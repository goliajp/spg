//! v7.39 (read01 round 99) — `trunc(numeric, N)` with a NEGATIVE N.
//!
//! The sibling of round 98's `round` fix: `trunc(1234.5678, -2)` returned the
//! right value (`1200`) but as `double precision`, while PG keeps `numeric`.
//! The exact branch only covered `0..=38`, so a negative target scale fell
//! through to the f64 path; an integer argument hit it too. Both now truncate
//! the mantissa toward zero and stay numeric. Values AND types locked against
//! live PG 18.4.

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
    assert_eq!(text(&mut e, "SELECT trunc(1234.5678, -2)::text"), "1200");
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(trunc(1234.5678, -2))::text"),
        "numeric"
    );
    assert_eq!(text(&mut e, "SELECT trunc(12345, -2)::text"), "12300");
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(trunc(12345, -2))::text"),
        "numeric"
    );
}

#[test]
fn negative_ndigits_truncates_toward_zero() {
    let mut e = Engine::new();
    // Truncation (not rounding): 1299 -> 1200, 50 -> 0, 999.9 -> 0.
    assert_eq!(text(&mut e, "SELECT trunc(1299, -2)::text"), "1200");
    assert_eq!(text(&mut e, "SELECT trunc(50, -2)::text"), "0");
    assert_eq!(text(&mut e, "SELECT trunc(999.9, -3)::text"), "0");
    assert_eq!(text(&mut e, "SELECT trunc(1234.5678, -1)::text"), "1230");
    // Toward zero for negatives, not toward -inf.
    assert_eq!(text(&mut e, "SELECT trunc(-1250, -2)::text"), "-1200");
    assert_eq!(text(&mut e, "SELECT trunc(-1234.5678, -2)::text"), "-1200");
}

#[test]
fn positive_and_float_paths_unchanged() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT trunc(1234.5678, 2)::text"), "1234.56");
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(trunc(1234.5678, 2))::text"),
        "numeric"
    );
    // 1-arg trunc on a genuine float stays double precision.
    assert_eq!(text(&mut e, "SELECT trunc(3.7::float8)::text"), "3");
}
