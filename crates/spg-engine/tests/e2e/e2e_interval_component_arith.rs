//! v7.38 (read01) — INTERVAL ± INTERVAL is purely component-wise: PG never
//! justifies days ↔ micros, so a mixed-sign result keeps its parts
//! (`1 day - 2 hours` = `1 day -02:00:00`) and micros may exceed a day
//! (`1 day - 26 hours` = `1 day -26:00:00`). Oracle: live PG 18.4.

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
fn interval_arithmetic_is_component_wise() {
    let mut e = Engine::new();
    let cases = [
        ("interval '1 day' - interval '2 hours'", "1 day -02:00:00"),
        ("interval '1 day' - interval '12 hours'", "1 day -12:00:00"),
        ("interval '1 day' - interval '26 hours'", "1 day -26:00:00"),
        ("interval '1 day' + interval '2 hours'", "1 day 02:00:00"),
        (
            "interval '2 days' - interval '30 hours'",
            "2 days -30:00:00",
        ),
        ("interval '1 mon' - interval '1 day'", "1 mon -1 days"),
        (
            "interval '1 mon 2 days 03:00:00' + interval '1 day 01:30:00'",
            "1 mon 3 days 04:30:00",
        ),
    ];
    for (expr, want) in cases {
        assert_eq!(
            text(&mut e, &format!("SELECT ({expr})::text")),
            want,
            "expr {expr}"
        );
    }
}
