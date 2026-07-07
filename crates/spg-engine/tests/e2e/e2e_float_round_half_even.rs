//! v7.38 (read01, T2) — FLOAT8 rounding uses half-to-even (banker's), matching
//! PG (`round(2.5::float8)=2`, `2.5::float8::int=2`), while a bare `2.5`
//! (a NUMERIC literal) still rounds half-away-from-zero (`round(2.5)=3`).
//! Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn cell(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => format!("{:?}", rows[0].values[0]),
        _ => panic!("expected rows"),
    }
}
fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn float8_rounds_half_to_even() {
    let mut e = Engine::new();
    // Explicit float8 → int / round: banker's rounding.
    assert_eq!(cell(&mut e, "SELECT 2.5::float8::int"), "Int(2)");
    assert_eq!(cell(&mut e, "SELECT 3.5::float8::int"), "Int(4)");
    assert_eq!(cell(&mut e, "SELECT (-2.5)::float8::int"), "Int(-2)");
    assert_eq!(cell(&mut e, "SELECT 0.5::float8::int"), "Int(0)");
    assert_eq!(cell(&mut e, "SELECT round(2.5::float8)"), "Float(2.0)");
    assert_eq!(cell(&mut e, "SELECT round(3.5::float8)"), "Float(4.0)");

    // A bare decimal literal is NUMERIC → half-away (unchanged).
    assert_eq!(cell(&mut e, "SELECT 2.5::int"), "Int(3)");
    assert_eq!(text(&mut e, "SELECT (round(2.5))::text"), "3");

    // float8[] → int[] narrows with half-to-even too.
    e.execute("CREATE TABLE fi (v int[])").unwrap();
    e.execute("INSERT INTO fi VALUES (ARRAY[2.5::float8, 3.5::float8, -0.5::float8])")
        .unwrap();
    assert_eq!(text(&mut e, "SELECT v::text FROM fi"), "{2,4,0}");
}
