//! v7.38 (read01) — make_date rejects an out-of-range day for the month
//! instead of silently rolling over (`make_date(2024,2,30)` must error, not
//! become 2024-03-01). February honours the leap year. Oracle: live PG 18.4.

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
fn make_date_validates_day_of_month() {
    let mut e = Engine::new();
    // Valid dates.
    assert_eq!(
        text(&mut e, "SELECT (make_date(2024,6,15))::text"),
        "2024-06-15"
    );
    assert_eq!(
        text(&mut e, "SELECT (make_date(2024,2,29))::text"),
        "2024-02-29"
    ); // leap
    assert_eq!(
        text(&mut e, "SELECT (make_date(2024,12,31))::text"),
        "2024-12-31"
    );
    // Out-of-range days / months error (no silent roll-over).
    assert!(e.execute("SELECT make_date(2024,2,30)").is_err());
    assert!(e.execute("SELECT make_date(2024,4,31)").is_err());
    assert!(e.execute("SELECT make_date(2023,2,29)").is_err()); // non-leap
    assert!(e.execute("SELECT make_date(2024,13,1)").is_err());
    assert!(e.execute("SELECT make_date(2024,6,0)").is_err());
}

#[test]
fn make_timestamp_validates_day_of_month() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT (make_timestamp(2024,6,15,14,30,45.5))::text"
        ),
        "2024-06-15 14:30:45.5"
    );
    assert_eq!(
        text(&mut e, "SELECT (make_timestamp(2024,2,29,0,0,0))::text"),
        "2024-02-29 00:00:00"
    );
    // Out-of-range date / time components error, no roll-over.
    assert!(
        e.execute("SELECT make_timestamp(2024,2,30,12,0,0)")
            .is_err()
    );
    assert!(e.execute("SELECT make_timestamp(2024,4,31,0,0,0)").is_err());
    assert!(
        e.execute("SELECT make_timestamp(2024,6,15,25,0,0)")
            .is_err()
    );
}
