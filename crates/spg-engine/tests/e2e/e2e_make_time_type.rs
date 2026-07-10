//! v7.38 (read01 sweep) — make_time returns TIME (not TIMESTAMP), and
//! pg_typeof reports a TIME value as "time without time zone". Oracle
//! behaviour from live PG 18.4.

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
fn make_time_is_time_typed() {
    let mut e = Engine::new();
    // Renders as a time, not "1970-01-01 12:30:00".
    assert_eq!(
        text(&mut e, "SELECT make_time(12, 30, 0)::text"),
        "12:30:00"
    );
    assert_eq!(
        text(&mut e, "SELECT make_time(8, 15, 30.5)::text"),
        "08:15:30.5"
    );
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(make_time(12,30,0))"),
        "time without time zone"
    );
    // Compares as a TIME.
    match e
        .execute("SELECT make_time(12,0,0) > '11:00:00'::time")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], spg_storage::Value::Bool(true))
        }
        _ => panic!(),
    }
}

#[test]
fn pg_typeof_reports_time() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT pg_typeof('12:30:00'::time)"),
        "time without time zone"
    );
}
