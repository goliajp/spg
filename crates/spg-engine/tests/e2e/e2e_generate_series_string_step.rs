//! v7.38 (read01 sweep) — generate_series(date/timestamp, ..., '<interval>')
//! accepts an unknown-type string step and resolves it to INTERVAL, matching
//! PG (`generate_series(date, date, '2 days')`). Oracle: live PG 18.4.
//!
//! v7.39 (read01 round 76) — the DATE-bound expectations here used to omit the
//! `+00` offset. They were pinning SPG's own output, not PG's: PG has no date
//! overload of generate_series and prefers the timestamptz candidate, so date
//! bounds come back `timestamp with time zone`. Re-probed against live PG18.4
//! and corrected. Timestamp bounds are unaffected — those stay TZ-naive.

use spg_engine::{Engine, QueryResult};

fn agg(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn generate_series_string_interval_step() {
    let mut e = Engine::new();
    // Date bounds + a bare string step (PG folds dates to midnight timestamps).
    assert_eq!(
        agg(
            &mut e,
            "SELECT string_agg(g::text, ',') FROM \
             generate_series('2024-01-01'::date, '2024-01-05'::date, '2 days') g"
        ),
        "2024-01-01 00:00:00+00,2024-01-03 00:00:00+00,2024-01-05 00:00:00+00"
    );
    // Timestamp bounds + string step.
    assert_eq!(
        agg(
            &mut e,
            "SELECT string_agg(g::text, ',') FROM \
             generate_series('2024-01-01'::timestamp, '2024-01-04'::timestamp, '1 day') g"
        ),
        "2024-01-01 00:00:00,2024-01-02 00:00:00,2024-01-03 00:00:00,2024-01-04 00:00:00"
    );
    // An explicit interval step still works, and an unparseable step errors.
    assert_eq!(
        agg(
            &mut e,
            "SELECT string_agg(g::text, ',') FROM \
             generate_series('2024-01-01'::date, '2024-01-03'::date, interval '2 days') g"
        ),
        "2024-01-01 00:00:00+00,2024-01-03 00:00:00+00"
    );
    assert!(
        e.execute(
            "SELECT * FROM generate_series('2024-01-01'::date, '2024-01-05'::date, 'garbage') g"
        )
        .is_err()
    );
}
