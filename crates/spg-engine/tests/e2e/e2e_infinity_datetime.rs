//! v7.38 (read01) — `infinity` / `-infinity` timestamp and date literals: they
//! render back as `infinity` / `-infinity` and compare greater/less than every
//! finite value (i64::MAX / i64::MIN and i32::MAX / i32::MIN sentinels).
//! Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            spg_storage::Value::Bool(b) => b.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn infinity_timestamp_and_date() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT ('infinity'::timestamp)::text"),
        "infinity"
    );
    assert_eq!(
        text(&mut e, "SELECT ('-infinity'::timestamp)::text"),
        "-infinity"
    );
    assert_eq!(text(&mut e, "SELECT ('infinity'::date)::text"), "infinity");
    assert_eq!(
        text(&mut e, "SELECT ('-infinity'::date)::text"),
        "-infinity"
    );
    // Ordering.
    assert_eq!(
        text(
            &mut e,
            "SELECT 'infinity'::timestamp > '2024-01-01'::timestamp"
        ),
        "true"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT '-infinity'::timestamp < '2024-01-01'::timestamp"
        ),
        "true"
    );
    assert_eq!(
        text(&mut e, "SELECT 'infinity'::date > '2024-01-01'::date"),
        "true"
    );
    // Finite values unaffected.
    assert_eq!(
        text(&mut e, "SELECT ('2024-06-15 12:00:00'::timestamp)::text"),
        "2024-06-15 12:00:00"
    );
    assert_eq!(
        text(&mut e, "SELECT ('2024-06-15'::date)::text"),
        "2024-06-15"
    );
}
