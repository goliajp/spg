//! v7.38 (read01) — `::time(N)` / `::timestamp(N)` / `::timestamptz(N)` round
//! the fractional-seconds field to N digits (half-away-from-zero, like PG's
//! AdjustTimestampForTypmod). Bare `::time` / `::timestamp` are unchanged.
//! Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

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
fn time_and_timestamp_precision_casts() {
    let mut e = Engine::new();
    let cases = [
        ("('12:34:56.789012'::time(0))::text", "12:34:57"),
        ("('12:34:56.789012'::time(3))::text", "12:34:56.789"),
        ("('12:34:56.789012'::time(6))::text", "12:34:56.789012"),
        ("('12:34:58.5'::time(0))::text", "12:34:59"), // half-away, not half-even
        (
            "('2024-06-15 12:34:56.789012'::timestamp(2))::text",
            "2024-06-15 12:34:56.79",
        ),
        (
            "('2024-06-15 12:34:56.789012'::timestamp(0))::text",
            "2024-06-15 12:34:57",
        ),
        (
            "('2024-06-15 12:34:56.789'::timestamptz(0))::text",
            "2024-06-15 12:34:57",
        ),
        ("('12:34:56.789'::time)::text", "12:34:56.789"), // bare cast unchanged
    ];
    for (expr, want) in cases {
        assert_eq!(text(&mut e, &format!("SELECT {expr}")), want, "expr {expr}");
    }
}
