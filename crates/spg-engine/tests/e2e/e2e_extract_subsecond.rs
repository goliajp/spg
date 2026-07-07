//! v7.38 (read01 sweep) — EXTRACT(epoch/second/milliseconds ...) keeps
//! sub-second precision as NUMERIC (PG), instead of truncating to an integer.
//! Oracle behaviour from live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn cast_text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn extract_keeps_subsecond_precision() {
    let mut e = Engine::new();
    assert_eq!(
        cast_text(&mut e, "SELECT extract(epoch FROM TIMESTAMP '2024-01-01 00:00:00.5')::text"),
        "1704067200.500000"
    );
    assert_eq!(
        cast_text(&mut e, "SELECT extract(second FROM TIMESTAMP '2024-01-01 00:00:30.75')::text"),
        "30.750000"
    );
    assert_eq!(
        cast_text(&mut e, "SELECT extract(milliseconds FROM TIMESTAMP '2024-01-01 00:00:30.75')::text"),
        "30750.000"
    );
    assert_eq!(
        cast_text(&mut e, "SELECT extract(epoch FROM INTERVAL '1.5 seconds')::text"),
        "1.500000"
    );
    // A whole-second value still renders its zero fraction.
    assert_eq!(
        cast_text(&mut e, "SELECT extract(second FROM TIMESTAMP '2024-01-01 00:00:30')::text"),
        "30.000000"
    );
}
