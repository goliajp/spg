//! v7.39 (round 246) — to_char/to_date/to_timestamp DATETIME template
//! sweep, 30 cases against live PG18.4 (2026-07-19), the date-side twin
//! of round 243. Day/Month names, FM/TM, IW/ID/DDD/WW/Q/J/CC, FF1-6,
//! MS/US, RM, ordinals, quoted literals, BC eras, two-digit-year pivots
//! and the ISO week-date parse all matched; the gaps:
//!
//!   * `SSSSS` (PG's alias for SSSS) left a stray literal `S`;
//!   * the zone tokens TZ / OF / TZH / TZM echoed themselves verbatim —
//!     they now answer for UTC, the zone SPG stores and renders
//!     everything in;
//!   * `to_char(TIME, …)` was rejected outright;
//!   * `to_date('2024-02-30', …)` ROLLED OVER SILENTLY to 2024-03-01 —
//!     the day now checks against the resolved month's real length, and
//!     the range errors take PG's wording quoting the original input;
//!   * non-FX parsing is whitespace-elastic in PG
//!     (`to_date('  05  03 2024','DD MM YYYY')`); SPG matched blanks
//!     positionally and failed.

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
fn sssss_and_zone_tokens() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT to_char(TIMESTAMP '2024-03-05 14:30:45', 'SSSS SSSSS')"
        ),
        "52245 52245"
    );
    // SPG stores and renders every timestamp in UTC; the zone tokens
    // answer for it (a session-zone-aware answer is the renderer's same
    // architecture step — recorded).
    assert_eq!(
        one(
            &mut e,
            "SELECT to_char(TIMESTAMPTZ '2024-03-05 14:30:45+00', 'TZ OF TZH:TZM')"
        ),
        "UTC +00 +00:00"
    );
}

#[test]
fn to_char_accepts_time() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT to_char(TIME '14:30:45', 'HH24:MI:SS')"),
        "14:30:45"
    );
    assert_eq!(
        one(&mut e, "SELECT to_char(TIME '02:30:45', 'HH12:MI AM')"),
        "02:30 AM"
    );
}

#[test]
fn to_date_validates_the_calendar() {
    let mut e = Engine::new();
    // Used to roll over silently to 2024-03-01.
    let got = err(&mut e, "SELECT to_date('2024-02-30', 'YYYY-MM-DD')");
    assert!(
        got.contains("date/time field value out of range: \"2024-02-30\""),
        "{got}"
    );
    let got = err(&mut e, "SELECT to_date('2024-13-05', 'YYYY-MM-DD')");
    assert!(
        got.contains("date/time field value out of range: \"2024-13-05\""),
        "{got}"
    );
    // Leap-year Feb 29 is legal; non-leap is not.
    assert_eq!(
        one(&mut e, "SELECT to_date('2024-02-29', 'YYYY-MM-DD')"),
        "2024-02-29"
    );
    let got = err(&mut e, "SELECT to_date('2023-02-29', 'YYYY-MM-DD')");
    assert!(got.contains("out of range: \"2023-02-29\""), "{got}");
}

#[test]
fn non_fx_parsing_is_whitespace_elastic() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT to_date('  05  03 2024', 'DD MM YYYY')"),
        "2024-03-05"
    );
    assert_eq!(
        one(&mut e, "SELECT to_date('05 03 2024', 'DD  MM  YYYY')"),
        "2024-03-05"
    );
}

#[test]
fn to_timestamp_is_timestamptz_and_core_unchanged() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_typeof(to_timestamp('2024-03-05','YYYY-MM-DD'))::text"
        ),
        "timestamp with time zone"
    );
    // Regression guard over the sweep's clean cases.
    for (sql, want) in [
        (
            "SELECT to_char(TIMESTAMP '2024-03-05 14:30:45', 'Day, DD Month YYYY')",
            "Tuesday  , 05 March     2024",
        ),
        (
            "SELECT to_char(DATE '2024-03-05', 'DDD IW ID D WW W Q')",
            "065 10 2 3 10 1 1",
        ),
        (
            "SELECT to_char(TIMESTAMP '2024-03-05 14:30:45.123456', 'MS US FF3')",
            "123 123456 123",
        ),
        ("SELECT to_char(DATE '2024-03-05', 'DDth Dth')", "05th 3rd"),
        ("SELECT to_date('05 Mar 2024', 'DD Mon YYYY')", "2024-03-05"),
        ("SELECT to_date('2024 12', 'IYYY IW')", "2024-03-18"),
        ("SELECT to_date('99-01-31', 'YY-MM-DD')", "1999-01-31"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}
