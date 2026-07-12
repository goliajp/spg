//! v7.38 (read01, T-timezone) — AT TIME ZONE with a common fixed-offset zone
//! abbreviation (EST/PST/JST/CET/…), which PG treats as a constant offset (no
//! DST variance). Full IANA named zones (America/New_York) still need tzdata.
//! Oracle: live PG 18.4 (session TZ = UTC).

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
fn at_time_zone_fixed_abbreviations() {
    let mut e = Engine::new();
    let base = "timestamp '2024-06-15 12:00:00'";
    // v7.39 (tz epic) — naive AT TIME ZONE types as timestamptz, so a
    // UTC session renders the +00 suffix; every value is PG18's.
    assert_eq!(
        text(&mut e, &format!("SELECT ({base} AT TIME ZONE 'EST')::text")),
        "2024-06-15 17:00:00+00"
    );
    assert_eq!(
        text(&mut e, &format!("SELECT ({base} AT TIME ZONE 'PST')::text")),
        "2024-06-15 20:00:00+00"
    );
    assert_eq!(
        text(&mut e, &format!("SELECT ({base} AT TIME ZONE 'JST')::text")),
        "2024-06-15 03:00:00+00"
    );
    assert_eq!(
        text(&mut e, &format!("SELECT ({base} AT TIME ZONE 'CET')::text")),
        "2024-06-15 11:00:00+00"
    );
    assert_eq!(
        text(&mut e, &format!("SELECT ({base} AT TIME ZONE 'UTC')::text")),
        "2024-06-15 12:00:00+00"
    );
    // A numeric offset still works.
    assert_eq!(
        text(
            &mut e,
            &format!("SELECT ({base} AT TIME ZONE '+05:00')::text")
        ),
        "2024-06-15 17:00:00+00"
    );
}
