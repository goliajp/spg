//! v7.39 (round 253) — the EXTRACT / date_part field surface, swept 68+
//! cases against live PG18.4 (2026-07-19). The component math (dow /
//! isodow / week / isoyear / quarter / century / millennium era
//! buckets, OVERLAPS, date_bin) already matched; the gaps:
//!
//!   * EXTRACT returned bigint where PG 14+ returns NUMERIC (and
//!     date_part double precision) — `extract(year …)/3` did integer
//!     division where PG gives `674.6666666666666667`;
//!   * `extract(hour FROM date)` (any time-of-day field on a DATE)
//!     silently answered 0 — PG refuses (0A000);
//!   * `extract(week FROM interval)` was rejected — PG answers days/7
//!     (truncating toward zero; probed 13d→1, -8d→-1);
//!   * JULIAN from a timestamp truncated the day fraction — PG renders
//!     the scale-20 numeric division form;
//!   * TIMETZ was not extractable at all (local clock fields, epoch
//!     minus the offset, signed timezone[_hour|_minute] parts), and the
//!     compact offset spellings ('+0230', '+023') did not parse;
//!   * an unknown field died at PARSE time with an internal listing —
//!     PG resolves fields at runtime: `unit "nosuch" not recognized for
//!     type timestamp without time zone` (22023); the not-supported
//!     family is 0A000, both now classified on the wire.
//!
//! Recorded residuals: `date_part('timezone', <timestamp col>)` stays 0
//! (the function form sees only the value; a tstz value shares
//! Value::Timestamp) — the EXTRACT form rejects via the declared type.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

#[test]
fn extract_is_numeric_and_date_part_is_float8() {
    let mut e = Engine::new();
    // The division that exposes the type: numeric vs integer division.
    assert_eq!(
        one(&mut e, "SELECT extract(year from timestamp '2024-03-15')/3"),
        "674.6666666666666667"
    );
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(extract(year from timestamp '2024-03-15'))"),
        "numeric"
    );
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(date_part('year', timestamp '2024-03-15'))"),
        "double precision"
    );
    assert_eq!(
        one(&mut e, "SELECT date_part('year', timestamp '2024-03-15')/3"),
        "674.6666666666666"
    );
    // Fraction-bearing fields keep their scales.
    assert_eq!(
        one(&mut e, "SELECT extract(epoch FROM timestamp '2024-03-15 14:30:45.123456')"),
        "1710513045.123456"
    );
    assert_eq!(
        one(&mut e, "SELECT extract(second FROM interval '1 minute 30.0005 seconds')"),
        "30.000500"
    );
}

#[test]
fn julian_carries_the_day_fraction() {
    let mut e = Engine::new();
    // PG renders the numeric division's scale-20 form for timestamps…
    assert_eq!(
        one(&mut e, "SELECT extract(julian FROM timestamp '2024-03-15 12:00:00')"),
        "2460385.50000000000000000000"
    );
    // …and the bare integer for dates.
    assert_eq!(one(&mut e, "SELECT extract(julian FROM date '2024-03-15')"), "2460385");
}

#[test]
fn field_type_validity_matches_pg() {
    let mut e = Engine::new();
    // DATE has no time-of-day (was silently 0).
    let got = err(&mut e, "SELECT extract(hour FROM date '2024-03-15')");
    assert!(got.contains("unit \"hour\" not supported for type date"), "{got}");
    // Plain timestamp has no timezone (was silently 0).
    let got = err(&mut e, "SELECT extract(timezone FROM timestamp '2024-03-15 14:30:45')");
    assert!(
        got.contains("unit \"timezone\" not supported for type timestamp without time zone"),
        "{got}"
    );
    // Interval: week IS supported (days/7, toward zero); dow is not.
    assert_eq!(one(&mut e, "SELECT extract(week FROM interval '30 days')"), "4");
    assert_eq!(one(&mut e, "SELECT extract(week FROM interval '13 days')"), "1");
    assert_eq!(one(&mut e, "SELECT extract(week FROM interval '-8 days')"), "-1");
    let got = err(&mut e, "SELECT extract(dow FROM interval '3 days')");
    assert!(got.contains("unit \"dow\" not supported for type interval"), "{got}");
    // Unknown field: runtime, with the source type (was a parse error).
    let got = err(&mut e, "SELECT extract(nosuch FROM timestamp '2024-03-15 00:00:00')");
    assert!(
        got.contains("unit \"nosuch\" not recognized for type timestamp without time zone"),
        "{got}"
    );
    let got = err(&mut e, "SELECT extract(nosuch FROM interval '3 days')");
    assert!(got.contains("unit \"nosuch\" not recognized for type interval"), "{got}");
    let got = err(&mut e, "SELECT date_part('nosuch', timestamp '2024-03-15 00:00:00')");
    assert!(
        got.contains("unit \"nosuch\" not recognized for type timestamp without time zone"),
        "{got}"
    );
}

#[test]
fn timetz_extracts_and_compact_offsets_parse() {
    let mut e = Engine::new();
    // Local clock fields; epoch subtracts the offset (probed live).
    assert_eq!(
        one(&mut e, "SELECT extract(second FROM timetz '14:30:45.5+02')"),
        "45.500000"
    );
    assert_eq!(
        one(&mut e, "SELECT extract(epoch FROM timetz '14:30:45.5+02')"),
        "45045.500000"
    );
    assert_eq!(
        one(&mut e, "SELECT extract(microsecond FROM timetz '14:30:45.5+02')"),
        "45500000"
    );
    // Signed offset parts; the compact '-0930' spelling parses.
    assert_eq!(
        one(&mut e, "SELECT extract(timezone_hour FROM timetz '14:30:45-0930')"),
        "-9"
    );
    assert_eq!(
        one(&mut e, "SELECT extract(timezone_minute FROM timetz '14:30:45-0930')"),
        "-30"
    );
    assert_eq!(
        one(&mut e, "SELECT extract(timezone FROM timetz '14:30:45+0230')"),
        "9000"
    );
    // The three-digit compact form is 0:MM (probed: '+023' = 00:23).
    assert_eq!(one(&mut e, "SELECT timetz '14:30:45+023'"), "14:30:45+00:23");
}

#[test]
fn the_component_core_is_pinned() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT extract(dow FROM timestamp '2024-03-15 14:30:45')", "5"),
        ("SELECT extract(isodow FROM timestamp '2024-03-17 14:30:45')", "7"),
        ("SELECT extract(week FROM timestamp '2024-01-01 00:00:00')", "1"),
        ("SELECT extract(week FROM date '2005-01-01')", "53"),
        ("SELECT extract(isoyear FROM timestamp '2024-01-01 00:00:00')", "2024"),
        ("SELECT extract(century FROM date '2000-12-31')", "20"),
        ("SELECT extract(century FROM date '0001-01-01')", "1"),
        ("SELECT extract(millennium FROM date '2000-12-31')", "2"),
        ("SELECT extract(decade FROM date '1999-12-31')", "199"),
        ("SELECT extract(quarter FROM interval '14 months')", "1"),
        ("SELECT extract(month FROM interval '3 years 14 months')", "2"),
        ("SELECT extract(epoch FROM timestamptz '2024-03-15 14:30:45+00')", "1710513045.000000"),
        (
            "SELECT date_bin('15 minutes', timestamp '2024-03-15 14:37:45', timestamp '2001-01-01 00:00:00')",
            "2024-03-15 14:30:00",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}
