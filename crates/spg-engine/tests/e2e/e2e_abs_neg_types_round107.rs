//! v7.39 (read01 round 107) — abs() / unary minus over the smaller numeric
//! types.
//!
//! `abs()` handled int4 / int8 / numeric / float8 but errored on int2
//! (`abs() needs numeric, got SmallInt`) and real; unary minus handled every
//! type except real (`unary - applied to Real`). Both now cover the full
//! numeric set, with the int2 overflow (`abs(-32768::int2)`) erroring like PG.
//! Locked byte-identical against live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn abs_covers_all_numeric_types() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT abs(-3::int2)::text"), "3");
    assert_eq!(text(&mut e, "SELECT abs(-3::int4)::text"), "3");
    assert_eq!(text(&mut e, "SELECT abs(-3::int8)::text"), "3");
    assert_eq!(text(&mut e, "SELECT abs(-3.5::numeric)::text"), "3.5");
    assert_eq!(text(&mut e, "SELECT abs(-3.5::float8)::text"), "3.5");
    assert_eq!(text(&mut e, "SELECT abs(-3.5::real)::text"), "3.5");
}

#[test]
fn abs_int2_overflow_errors_like_pg() {
    let mut e = Engine::new();
    let err = e
        .execute("SELECT abs(-32768::int2)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("smallint out of range"),
        "unexpected error: {err}"
    );
}

#[test]
fn unary_minus_covers_all_numeric_types() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT (-(5::int2))::text"), "-5");
    assert_eq!(text(&mut e, "SELECT (-(5::int4))::text"), "-5");
    assert_eq!(text(&mut e, "SELECT (-(5::int8))::text"), "-5");
    assert_eq!(text(&mut e, "SELECT (-(5.5::numeric))::text"), "-5.5");
    assert_eq!(text(&mut e, "SELECT (-(5.5::float8))::text"), "-5.5");
    assert_eq!(text(&mut e, "SELECT (-(5.5::real))::text"), "-5.5");
}
