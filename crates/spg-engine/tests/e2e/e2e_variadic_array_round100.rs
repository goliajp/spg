//! v7.39 (read01 round 100) — `VARIADIC <array>` argument spreading.
//!
//! PG lets you pass an array to a variadic function's trailing parameter with
//! `VARIADIC`: `concat_ws(',', VARIADIC ARRAY[…])` spreads the array's elements
//! as individual arguments. SPG's parser rejected the `VARIADIC` keyword
//! outright. The parser now records it (`Expr::Variadic`) and the evaluator
//! splices the array's elements into the call before dispatch. Results locked
//! byte-identical against live PG 18.4.
//!
//! (PG rejects mixing individual variadic args with a VARIADIC array, e.g.
//! `concat_ws('-', 'x', VARIADIC arr)`; SPG is leniently permissive there —
//! matching PG's exact rejection needs per-function arity knowledge. Deferred.)

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn variadic_array_spreads_into_the_call() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT concat(VARIADIC ARRAY['a','b','c'])"), "abc");
    assert_eq!(text(&mut e, "SELECT concat(VARIADIC ARRAY[1,2,3])"), "123");
    assert_eq!(
        text(&mut e, "SELECT concat_ws('-', VARIADIC ARRAY[1,2,3])"),
        "1-2-3"
    );
    assert_eq!(
        text(&mut e, "SELECT format('%s/%s/%s', VARIADIC ARRAY['x','y','z'])"),
        "x/y/z"
    );
}

#[test]
fn variadic_empty_array_contributes_nothing() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT concat_ws(',', VARIADIC ARRAY[]::text[])"),
        ""
    );
    assert_eq!(text(&mut e, "SELECT concat(VARIADIC ARRAY[]::text[])"), "");
}

#[test]
fn variadic_round_trips_through_display() {
    use spg_sql::parser::parse_statement;
    let stmt = parse_statement("SELECT concat_ws(',', VARIADIC ARRAY[1, 2])").unwrap();
    let rendered = stmt.to_string();
    assert!(
        rendered.contains("VARIADIC ARRAY[1, 2]"),
        "unexpected Display: {rendered}"
    );
}
