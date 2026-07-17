//! v7.39 (read01 round 185) — ORDER BY binds to the OUTPUT column
//! when its implicit label matches, like PG.
//!
//! A cast keeps its inner column's label (`SELECT x::text` is named
//! `x`), a function call is named after the function — and PG's
//! `ORDER BY <name>` prefers the output column over the source
//! column. Pre-r185 SPG only matched EXPLICIT aliases, so
//! `SELECT x::text FROM v ORDER BY x` silently sorted by the source
//! int instead of the projected text (live-PG18 differential
//! 2026-07-18: PG 10,2 — text order; SPG 2,10 — int order).

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Text(s) => s.to_string(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn cast_output_label_binds_order_by() {
    let mut e = Engine::new();
    // PG: text order — "10" < "2".
    assert_eq!(
        col(&mut e, "SELECT x::text FROM (VALUES (2),(10)) v(x) ORDER BY x"),
        ["10", "2"]
    );
}

#[test]
fn function_output_label_binds_order_by() {
    let mut e = Engine::new();
    // Output label of upper(s) is "upper"; ORDER BY upper sorts by it.
    assert_eq!(
        col(
            &mut e,
            "SELECT upper(s) FROM (VALUES ('b'),('A'),('c')) v(s) ORDER BY upper"
        ),
        ["A", "B", "C"]
    );
}

#[test]
fn explicit_alias_still_wins() {
    let mut e = Engine::new();
    // Explicit alias behavior unchanged (was already correct).
    assert_eq!(
        col(
            &mut e,
            "SELECT x::text AS t FROM (VALUES (2),(10)) v(x) ORDER BY t"
        ),
        ["10", "2"]
    );
    // ORDER BY the SOURCE column name still reaches the source when
    // no output label matches.
    assert_eq!(
        col(
            &mut e,
            "SELECT x::text AS t FROM (VALUES (2),(10)) v(x) ORDER BY x"
        ),
        ["2", "10"]
    );
}

#[test]
fn bare_column_projection_unchanged() {
    let mut e = Engine::new();
    // `SELECT x … ORDER BY x` — label == source; int order as ever.
    assert_eq!(
        col(&mut e, "SELECT x FROM (VALUES (2),(10)) v(x) ORDER BY x"),
        ["Int(2)", "Int(10)"]
    );
}
