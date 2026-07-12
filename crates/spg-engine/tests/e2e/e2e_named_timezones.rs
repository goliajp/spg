//! v7.39 (tz epic) — named IANA timezones end to end: SET validation
//! + canonicalisation, per-value DST rendering, wall-clock input in
//! the session zone, AT TIME ZONE both directions, EXTRACT(timezone).
//! Every expected value is the live PG18 oracle's output.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.set_tz_fns(
        spg_tzif::tz_offset_at,
        spg_tzif::tz_local_to_utc,
        spg_tzif::tz_canonical,
        spg_tzif::tz_abbrev_at,
    );
    e
}

fn text_of(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn named_zone_set_validates_and_canonicalises() {
    let mut e = engine();
    e.execute("SET timezone = 'asia/tokyo'").unwrap();
    assert_eq!(e.session_param("timezone"), Some("Asia/Tokyo"));
    e.execute("SET timezone = 'utc'").unwrap();
    assert_eq!(e.session_param("timezone"), Some("UTC"));
    let err = e.execute("SET timezone = 'Bogus/Zone'").unwrap_err();
    assert!(
        format!("{err}").contains("invalid value for parameter \"TimeZone\""),
        "got {err}"
    );
}

#[test]
fn tstz_cast_renders_per_value_dst_offsets() {
    let mut e = engine();
    e.execute("SET timezone = 'America/New_York'").unwrap();
    // One session, two instants, two offsets (winter -05, summer -04).
    assert_eq!(
        text_of(&mut e, "SELECT (TIMESTAMPTZ '2024-01-15 12:00:00+00')::text"),
        "2024-01-15 07:00:00-05"
    );
    assert_eq!(
        text_of(&mut e, "SELECT (TIMESTAMPTZ '2024-07-15 12:00:00+00')::text"),
        "2024-07-15 08:00:00-04"
    );
}

#[test]
fn offsetless_tstz_literal_reads_session_wall_clock() {
    let mut e = engine();
    e.execute("SET timezone = 'America/New_York'").unwrap();
    assert_eq!(
        text_of(&mut e, "SELECT (TIMESTAMPTZ '2024-07-15 12:00:00')::text"),
        "2024-07-15 12:00:00-04"
    );
    // Trailing zone name localises there instead.
    assert_eq!(
        text_of(&mut e, "SELECT (TIMESTAMPTZ '2024-07-15 12:00 Asia/Tokyo')::text"),
        "2024-07-14 23:00:00-04"
    );
}

#[test]
fn at_time_zone_both_directions_and_extract() {
    let mut e = engine();
    e.execute("SET timezone = 'America/New_York'").unwrap();
    // naive AT ZONE -> timestamptz (rendered in the session zone).
    assert_eq!(
        text_of(
            &mut e,
            "SELECT (TIMESTAMP '2024-07-15 12:00' AT TIME ZONE 'Asia/Tokyo')::text"
        ),
        "2024-07-14 23:00:00-04"
    );
    // tstz AT ZONE -> naive wall clock in that zone.
    assert_eq!(
        text_of(
            &mut e,
            "SELECT TIMESTAMPTZ '2024-07-15 12:00+00' AT TIME ZONE 'Asia/Tokyo'"
        ),
        "2024-07-15 21:00:00"
    );
    assert_eq!(
        text_of(
            &mut e,
            "SELECT EXTRACT(timezone FROM TIMESTAMPTZ '2024-07-15 12:00+00')"
        ),
        "-14400"
    );
}
