//! v7.39 (read01 round 106) — aggregates in a FROM-less SELECT.
//!
//! `SELECT count(*)` / `SELECT sum(5)` / `SELECT string_agg('x', ',')` run the
//! aggregate over the single implicit row. SPG fell through to the scalar
//! projection, where the aggregate name looked like an unknown function
//! (`unknown function count_star`). The FROM-less path now routes an aggregate
//! projection through the aggregate executor over that one row (WHERE-filtered,
//! so `… WHERE false` leaves it zero input rows). Locked byte-identical
//! against live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(|v| match v {
                spg_storage::Value::Null => "NULL".to_string(),
                _ => spg_engine::eval::value_to_text(v),
            })
            .collect::<Vec<_>>()
            .join("|"),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn common_aggregates_over_the_implicit_row() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT count(*)"), "1");
    assert_eq!(
        text(
            &mut e,
            "SELECT sum(5), max(3), min(7), avg(4)::text, (array_agg(5))::text"
        ),
        "5|3|7|4.0000000000000000|{5}"
    );
    assert_eq!(
        text(&mut e, "SELECT string_agg('x', ','), count(1), count(NULL)"),
        "x|1|0"
    );
    // (embedded value_to_text renders bool as true/false; the wire form is t/f)
    assert_eq!(
        text(&mut e, "SELECT bool_and(true), bool_or(false)"),
        "true|false"
    );
}

#[test]
fn where_filters_the_implicit_row() {
    let mut e = Engine::new();
    // WHERE false leaves zero input rows: count → 0, sum → NULL.
    assert_eq!(text(&mut e, "SELECT count(*) WHERE false"), "0");
    assert_eq!(text(&mut e, "SELECT sum(5) WHERE false"), "NULL");
    assert_eq!(text(&mut e, "SELECT count(*) WHERE true"), "1");
}

#[test]
fn filter_having_and_ordered_agg() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT count(*) FILTER (WHERE false), count(*) FILTER (WHERE true)"
        ),
        "0|1"
    );
    assert_eq!(text(&mut e, "SELECT count(*) HAVING count(*) > 0"), "1");
    assert_eq!(text(&mut e, "SELECT string_agg('a', ',' ORDER BY 1)"), "a");
}

#[test]
fn non_aggregate_constant_select_unaffected() {
    // Regression guard: a plain constant SELECT still evaluates as scalars.
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT 1 + 1, 'x', coalesce(NULL, 3)"),
        "2|x|3"
    );
    // A non-aggregate `SELECT … WHERE false` still returns zero rows.
    match e.execute("SELECT 1 WHERE false").unwrap() {
        QueryResult::Rows { rows, .. } => assert!(rows.is_empty()),
        other => panic!("{other:?}"),
    }
}
