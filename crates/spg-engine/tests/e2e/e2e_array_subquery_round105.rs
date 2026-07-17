//! v7.39 (read01 round 105) — the `ARRAY(<subquery>)` constructor.
//!
//! `ARRAY(SELECT …)` gathers a subquery's single-column rows, in the
//! subquery's row order, into an array. SPG's parser rejected it (`unexpected
//! token Select in expression`) — only `ARRAY[…]` was recognised. It now
//! desugars to a scalar subquery `SELECT array_agg(c) FROM (<subquery>) t(c)`,
//! reusing the existing ScalarSubquery machinery, so ORDER BY / LIMIT / WHERE /
//! UNION / WITH inside the subquery all carry through. Locked byte-identical
//! against live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn array_subquery_collects_rows_in_order() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT (ARRAY(SELECT generate_series(1,3)))::text"),
        "{1,2,3}"
    );
    // Lowercase spelling + a descending series preserves order.
    assert_eq!(
        text(
            &mut e,
            "SELECT (array(SELECT x FROM generate_series(3,1,-1) x))::text"
        ),
        "{3,2,1}"
    );
}

#[test]
fn array_subquery_honors_where_order_limit() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT (ARRAY(SELECT x FROM generate_series(1,5) x WHERE x%2=1 ORDER BY x DESC))::text"
        ),
        "{5,3,1}"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT (ARRAY(SELECT x FROM generate_series(1,5) x ORDER BY x DESC LIMIT 3))::text"
        ),
        "{5,4,3}"
    );
}

#[test]
fn array_subquery_union_and_with() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT (ARRAY(SELECT 'a' UNION SELECT 'b' ORDER BY 1))::text"
        ),
        "{a,b}"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT (ARRAY(WITH t AS (SELECT 5 AS n) SELECT n FROM t))::text"
        ),
        "{5}"
    );
}

#[test]
fn array_literal_constructor_unaffected() {
    // Regression guard: ARRAY[...] (the literal form) must still parse.
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT ARRAY[1,2,3]::text"), "{1,2,3}");
    assert_eq!(
        text(&mut e, "SELECT ARRAY[[1,2],[3,4]]::text"),
        "{{1,2},{3,4}}"
    );
}
