//! v7.39 (round 309, V30) — a named zone is legal input to the
//! zone-LESS types, and PG throws the zone away.
//!
//! `'2020-01-01 10:00:00 America/New_York'::timestamp` is 10:00, not
//! 15:00 — the target has no zone to convert into, so the wall clock
//! stands. Round 289 established that for a numeric `+02` offset; a
//! named zone still failed to parse at all, which is why V30 was filed
//! as "wrong but loud".
//!
//! The zone is validated, not merely stripped, and PG splits the two
//! failure modes on SHAPE — measured, not assumed:
//!
//!   * a path-shaped token it does not know is a misspelt ZONE:
//!     `time zone "bogus/zone" not recognized`;
//!   * a bare word it does not know makes the whole literal invalid
//!     syntax — nothing marked it as having meant a zone.
//!
//! Validation is why this lives on the context-aware cast path:
//! resolving a zone needs the host tz functions, and `cast_value` has
//! no context. Every expectation read off live PG 18.4 (2026-07-21).

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

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
    }
}

#[test]
fn a_named_zone_is_discarded_by_the_zoneless_types() {
    let mut e = engine();
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00 America/New_York'::timestamp"),
        "2020-01-01 10:00:00"
    );
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00 America/New_York'::date"),
        "2020-01-01"
    );
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00 America/New_York'::time"),
        "10:00:00"
    );
    // A date-only literal with a zone lands at midnight.
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 America/New_York'::date"),
        "2020-01-01"
    );
    // Discarding is unconditional — a summer instant is not shifted
    // either, even though the zone's offset differs there.
    assert_eq!(
        one(&mut e, "SELECT '2020-07-01 10:00:00 America/New_York'::timestamp"),
        "2020-07-01 10:00:00"
    );
    // Abbreviations and multi-segment zone names both resolve.
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00 CET'::timestamp"),
        "2020-01-01 10:00:00"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT '2020-01-01 10:00:00 America/Indiana/Knox'::timestamp"
        ),
        "2020-01-01 10:00:00"
    );
    // The zone name is matched case-insensitively.
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00 america/new_york'::timestamp"),
        "2020-01-01 10:00:00"
    );
}

/// The half that makes this a validation rather than a strip.
#[test]
fn an_unknown_zone_name_is_named_in_the_error() {
    let mut e = engine();
    for sql in [
        "SELECT '2020-01-01 10:00:00 Bogus/Zone'::timestamp",
        "SELECT '2020-01-01 10:00:00 Bogus/Zone'::date",
        "SELECT '2020-01-01 10:00:00 Bogus/Zone'::time",
    ] {
        assert!(
            err(&mut e, sql).contains("time zone \"bogus/zone\" not recognized"),
            "{sql}\n  got: {}",
            err(&mut e, sql)
        );
    }
    // Lowercased in the message regardless of how it was written.
    assert!(
        err(&mut e, "SELECT '2020-01-01 10:00:00 Foo/Bar'::timestamp")
            .contains("time zone \"foo/bar\" not recognized")
    );
}

/// A bare word PG does not know is NOT reported as a zone — nothing
/// marked it as having meant one, so the literal is simply malformed.
/// Getting this wrong would answer "not recognized" for a typo that has
/// nothing to do with time zones.
#[test]
fn an_unknown_bare_word_is_a_malformed_literal_not_a_zone() {
    let mut e = engine();
    for sql in [
        "SELECT '2020-01-01 10:00:00 xyz'::timestamp",
        "SELECT '2020-01-01 10:00:00 ABCD'::timestamp",
        "SELECT '2020-01-01 10:00:00 UTC_X'::timestamp",
    ] {
        let msg = err(&mut e, sql);
        assert!(
            !msg.contains("not recognized"),
            "{sql} must not be reported as a zone\n  got: {msg}"
        );
    }
}

/// A bare TIME literal does not accept a named zone in PG — the zone
/// only rides along on a full timestamp. Must stay an error.
#[test]
fn a_bare_time_literal_still_refuses_a_named_zone() {
    let mut e = engine();
    assert!(!err(&mut e, "SELECT '10:00:00 America/New_York'::time").is_empty());
}

/// The zone-BEARING target is unchanged: it converts rather than
/// discards. Both halves have to hold, or the fix has just moved the
/// bug to the other type.
#[test]
fn timestamptz_still_converts_rather_than_discarding() {
    let mut e = engine();
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00 America/New_York'::timestamptz"),
        "2020-01-01 15:00:00"
    );
    // And round 289's numeric-offset behaviour is untouched.
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00+02'::timestamp"),
        "2020-01-01 10:00:00"
    );
}
