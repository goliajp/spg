//! v7.38 (read01 sweep) — to_timestamp / to_date honour the WW (week-of-year)
//! template field. PG's WW: week 1 starts Jan 1, each week is 7 days, so the
//! week's first day is day-of-year (W-1)*7+1. Oracle from live PG 18.4.

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
fn to_timestamp_and_to_date_honour_ww() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT to_timestamp('2023 15', 'YYYY WW')::text"),
        "2023-04-09 00:00:00+00"
    );
    // Week 1 is Jan 1.
    assert_eq!(
        text(&mut e, "SELECT to_timestamp('2023 1', 'YYYY WW')::text"),
        "2023-01-01 00:00:00+00"
    );
    assert_eq!(
        text(&mut e, "SELECT to_date('2020 10', 'YYYY WW')::text"),
        "2020-03-04"
    );
    // DDD (day-of-year) still resolves via the same post-loop path.
    assert_eq!(
        text(&mut e, "SELECT to_date('2023 100', 'YYYY DDD')::text"),
        "2023-04-10"
    );
}
