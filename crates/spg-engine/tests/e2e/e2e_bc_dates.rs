//! v7.39 (GUC knife 6) — BC dates end to end, differential-locked
//! against PG18: input (`0044-03-15 BC`, era after the TIME part for
//! timestamps, no year zero), output (every DateStyle carries the
//! " BC" suffix; offset before " BC" for timestamptz), arithmetic
//! across the era boundary, and era-year EXTRACT reporting.

use spg_engine::{Engine, QueryResult};

fn text_of(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn bc_dates_round_trip_and_compute() {
    let mut e = Engine::new();
    assert_eq!(text_of(&mut e, "SELECT DATE '0044-03-15 BC'"), "0044-03-15 BC");
    assert_eq!(
        text_of(&mut e, "SELECT DATE '0044-03-15 BC' + 10"),
        "0044-03-25 BC"
    );
    assert_eq!(
        text_of(&mut e, "SELECT TIMESTAMP '0044-03-15 10:20:30 BC'"),
        "0044-03-15 10:20:30 BC"
    );
    // 1 BC is a leap year (astronomical year 0): 366 days to 1 AD.
    assert_eq!(
        text_of(&mut e, "SELECT DATE '0001-01-01 BC' - DATE '0001-01-01'"),
        "-366"
    );
    // Era-year reporting: no year zero, so 44 BC extracts as -44.
    assert_eq!(
        text_of(&mut e, "SELECT EXTRACT(year FROM DATE '0044-03-15 BC')"),
        "-44"
    );
    assert_eq!(
        text_of(&mut e, "SELECT EXTRACT(century FROM DATE '0044-03-15 BC')"),
        "-1"
    );
    // An explicit AD parses and is the default era.
    assert_eq!(text_of(&mut e, "SELECT DATE '2024-03-15 AD'"), "2024-03-15");
    // Year zero does not exist.
    let err = e.execute("SELECT DATE '0000-01-01'").unwrap_err();
    assert!(
        format!("{err}").contains("out of range"),
        "expected out-of-range, got {err}"
    );
    // Styled output keeps the era suffix.
    e.execute("SET datestyle = 'German'").unwrap();
    assert_eq!(text_of(&mut e, "SELECT (DATE '0044-03-15 BC')::text"), "15.03.0044 BC");
}
