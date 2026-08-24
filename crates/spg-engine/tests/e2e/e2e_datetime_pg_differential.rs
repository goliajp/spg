//! Date/time/interval differential corrections vs PostgreSQL 18.
//!
//! Every expected value in this file was captured live from PG 18.4
//! (`psql -tAc`). It guards three CLEAR-BUG fixes found by a
//! differential sweep of the date/time surface:
//!
//!   1. `extract(hour FROM interval)` no longer wraps mod 24 — PG
//!      keeps interval hours unbounded (days are a separate field).
//!   2. `interval <cmp> interval` now compares by PG's canonical
//!      microsecond span (month = 30 days, day = 24 h).
//!   3. `to_char(<date/ts>, …)` grew the missing PG day/month/week
//!      name + number tokens (Day/DY/MON/Q/WW/IW/DDD/D/ID/J/CC/FM/…).
//!
//! Divergences deliberately NOT fixed (documented, not asserted
//! equal — see the module-level notes at the bottom).

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

/// Render the first scalar of the first row as PG-comparable text.
fn scalar(e: &mut Engine, sql: &str) -> String {
    use spg_engine::eval as f;
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("{sql}: expected Rows");
    };
    match &rows[0].values[0] {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "t" } else { "f" }.into(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Text(s) => s.to_string(),
        Value::Date(d) => f::format_date(*d),
        Value::Timestamp(t) => f::format_timestamp(*t),
        Value::Interval {
            months,
            days,
            micros,
            kind,
        } => f::format_interval(*months, *days, *micros),
        // v7.39 (round 253) — EXTRACT returns numeric (PG 14+).
        Value::Numeric { .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: unexpected {other:?}"),
    }
}

#[test]
fn extract_hour_from_interval_is_unbounded() {
    let mut e = Engine::new();
    // PG: extract(hour from interval '25 hours') = 25 (not 1).
    assert_eq!(
        scalar(&mut e, "SELECT extract(hour FROM INTERVAL '25 hours')"),
        "25"
    );
    assert_eq!(
        scalar(&mut e, "SELECT extract(hour FROM INTERVAL '50 hours')"),
        "50"
    );
    // Days are a separate dimension — they never roll into hours.
    assert_eq!(
        scalar(
            &mut e,
            "SELECT extract(hour FROM INTERVAL '1 day 25 hours')"
        ),
        "25"
    );
    // MINUTE / SECOND still wrap mod 60 (HH:MM:SS keeps MM/SS < 60).
    assert_eq!(
        scalar(&mut e, "SELECT extract(minute FROM INTERVAL '90 minutes')"),
        "30"
    );
    assert_eq!(
        scalar(&mut e, "SELECT extract(minute FROM INTERVAL '150 minutes')"),
        "30"
    );
}

#[test]
fn interval_comparison_pg_canonical_span() {
    let mut e = Engine::new();
    // PG canonicalises month = 30 days, day = 24 h, year = 360 days.
    assert_eq!(
        scalar(&mut e, "SELECT INTERVAL '1 month' = INTERVAL '30 days'"),
        "t"
    );
    assert_eq!(
        scalar(&mut e, "SELECT INTERVAL '1 day' = INTERVAL '24 hours'"),
        "t"
    );
    assert_eq!(
        scalar(&mut e, "SELECT INTERVAL '2 mons' = INTERVAL '60 days'"),
        "t"
    );
    assert_eq!(
        scalar(&mut e, "SELECT INTERVAL '1 year' = INTERVAL '360 days'"),
        "t"
    );
    assert_eq!(
        scalar(&mut e, "SELECT INTERVAL '1 month' = INTERVAL '31 days'"),
        "f"
    );
    assert_eq!(
        scalar(&mut e, "SELECT INTERVAL '1 month' > INTERVAL '29 days'"),
        "t"
    );
    assert_eq!(
        scalar(&mut e, "SELECT INTERVAL '2 hours' < INTERVAL '1 day'"),
        "t"
    );
}

#[test]
fn to_char_date_name_tokens() {
    let mut e = Engine::new();
    // Reference instant: 2024-03-05 09:07:03 — a Tuesday in March.
    let ts = "TIMESTAMP '2024-03-05 09:07:03'";
    let f = |e: &mut Engine, tok: &str| scalar(e, &format!("SELECT to_char({ts}, '{tok}')"));
    // Full day / month names are blank-padded to 9, case-templated.
    assert_eq!(f(&mut e, "Day"), "Tuesday  ");
    assert_eq!(f(&mut e, "DAY"), "TUESDAY  ");
    assert_eq!(f(&mut e, "day"), "tuesday  ");
    assert_eq!(f(&mut e, "Month"), "March    ");
    assert_eq!(f(&mut e, "MONTH"), "MARCH    ");
    assert_eq!(f(&mut e, "month"), "march    ");
    // Abbreviated names (no padding).
    assert_eq!(f(&mut e, "Dy"), "Tue");
    assert_eq!(f(&mut e, "DY"), "TUE");
    assert_eq!(f(&mut e, "dy"), "tue");
    assert_eq!(f(&mut e, "Mon"), "Mar");
    assert_eq!(f(&mut e, "MON"), "MAR");
    assert_eq!(f(&mut e, "mon"), "mar");
}

#[test]
fn to_char_date_number_tokens() {
    let mut e = Engine::new();
    let ts = "TIMESTAMP '2024-03-05 09:07:03'";
    let f = |e: &mut Engine, tok: &str| scalar(e, &format!("SELECT to_char({ts}, '{tok}')"));
    assert_eq!(f(&mut e, "Q"), "1"); // quarter
    assert_eq!(f(&mut e, "WW"), "10"); // week of year
    assert_eq!(f(&mut e, "IW"), "10"); // ISO week
    assert_eq!(f(&mut e, "DDD"), "065"); // day of year
    assert_eq!(f(&mut e, "D"), "3"); // day of week, Sunday = 1
    assert_eq!(f(&mut e, "ID"), "2"); // ISO day of week, Monday = 1
    assert_eq!(f(&mut e, "W"), "1"); // week of month
    assert_eq!(f(&mut e, "J"), "2460375"); // Julian day
    assert_eq!(f(&mut e, "CC"), "21"); // century
    assert_eq!(f(&mut e, "HH"), "09"); // HH is HH12
    assert_eq!(f(&mut e, "IYYY"), "2024"); // ISO year
    assert_eq!(f(&mut e, "RM"), "III "); // roman month, padded to 4
    assert_eq!(f(&mut e, "rm"), "iii ");
}

#[test]
fn to_char_fm_fill_mode() {
    let mut e = Engine::new();
    let ts = "TIMESTAMP '2024-03-05 09:07:03'";
    let f = |e: &mut Engine, tok: &str| scalar(e, &format!("SELECT to_char({ts}, '{tok}')"));
    // FM drops the zero pad on numbers and the blank pad on names.
    assert_eq!(f(&mut e, "FMDD"), "5");
    assert_eq!(f(&mut e, "FMDay"), "Tuesday");
    assert_eq!(f(&mut e, "FMMonth"), "March");
    assert_eq!(f(&mut e, "FMMonth FMDD, YYYY"), "March 5, 2024");
}

#[test]
fn to_char_unchanged_tokens_regress() {
    let mut e = Engine::new();
    // Guard the pre-existing token set stays correct after the rewrite.
    assert_eq!(
        scalar(
            &mut e,
            "SELECT to_char(TIMESTAMP '2024-03-05 09:07:03', 'YYYY-MM-DD HH24:MI:SS')"
        ),
        "2024-03-05 09:07:03"
    );
    assert_eq!(
        scalar(
            &mut e,
            "SELECT to_char(TIMESTAMP '2024-03-05 14:07:03', 'HH12:MI AM')"
        ),
        "02:07 PM"
    );
    assert_eq!(
        scalar(
            &mut e,
            "SELECT to_char(TIMESTAMP '2024-01-01 00:00:00.123', 'HH24:MI:SS.MS')"
        ),
        "00:00:00.123"
    );
}

// -------------------------------------------------------------------
// Deferred divergences (documented, NOT asserted equal to PG):
//
//  * EXTRACT(second/milliseconds/epoch …) returns an integer BigInt,
//    PG returns fractional `numeric` (e.g. second → 45.500000). The
//    numeric VALUE matches; the fractional formatting does not.
//    Fixing needs changing EXTRACT's return type surface — SEMANTIC.
//
//  * `date + interval` stays a DATE for whole-day/month intervals;
//    PG always promotes to `timestamp`. The instant is identical
//    (2024-02-29 vs 2024-02-29 00:00:00). SPG's date-stays-date is a
//    deliberate design with dedicated unit tests — SEMANTIC.
//
//  * `timestamp - timestamp` returns BigInt microseconds; PG returns
//    an `interval` ("60 days 12:00:00"). Correcting the value type
//    also needs describe.rs binop type inference updated in lockstep
//    — reported SEMANTIC, not forced here.
//
//  * `interval '1.5 months'` (fractional interval literals) errors;
//    PG cascades the fraction down a unit with per-unit rounding
//    rules (DecodeInterval). Faithful port is non-localized — SEMANTIC.
//
//  * `age()` produces a day-granular interval, not PG's month/year
//    justified form ("60 days" vs "2 mons"). Deliberate per the age()
//    doc-comment — SEMANTIC.
//
//  * `to_char(interval, …)` errors; PG formats the micros component
//    as time-of-day with interval-specific token semantics. Needs a
//    separate interval-aware to_char path — SEMANTIC.
//
//  * `date_trunc('day', DATE …)` renders without a `+00` suffix; PG
//    resolves the `timestamptz` overload for a bare date. Value is
//    correct — KNOWN-LIMITATION (SPG has no real-offset timestamptz).
// -------------------------------------------------------------------

#[test]
fn to_char_interval_year_digit_forms() {
    // to_char(interval, …) understands the trailing-N-digit year codes
    // (live PG18.4: YYYY '0005', YYY '001', YY '01', Y '1'; YY of 123
    // years wraps to '23'). Previously only YYYY was handled and YY/YYY/Y
    // fell through as literal text.
    let mut e = Engine::new();
    assert_eq!(
        scalar(
            &mut e,
            "SELECT to_char(INTERVAL '1 year 2 months', 'YY-MM')"
        ),
        "01-02"
    );
    assert_eq!(
        scalar(&mut e, "SELECT to_char(INTERVAL '1 year 2 months', 'YYY')"),
        "001"
    );
    assert_eq!(
        scalar(&mut e, "SELECT to_char(INTERVAL '1 year', 'Y')"),
        "1"
    );
    assert_eq!(
        scalar(&mut e, "SELECT to_char(INTERVAL '123 years', 'YY')"),
        "23"
    );
    // YYYY and the time codes are unaffected.
    assert_eq!(
        scalar(&mut e, "SELECT to_char(INTERVAL '5 years', 'YYYY')"),
        "0005"
    );
    assert_eq!(
        scalar(
            &mut e,
            "SELECT to_char(INTERVAL '2 hours 30 minutes', 'HH24:MI')"
        ),
        "02:30"
    );
}
