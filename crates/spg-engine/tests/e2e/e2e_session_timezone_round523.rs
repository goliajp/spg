//! v7.39 (round 523) — the session zone reaches past the renderer.
//!
//! Round 522 recorded that `SET TimeZone` did not reach
//! `to_char(timestamptz, 'TZ')`. Measuring that found one cause with
//! eight faces: the zone was applied when a value was PRINTED and
//! nowhere else. PG applies it wherever an instant meets the local
//! calendar — reading a field, truncating, formatting, casting either
//! direction, and reading the clock.
//!
//! Measured against PG18 under `SET TimeZone = 'Asia/Tokyo'`:
//!
//!     to_char(tstz,'HH24:MI TZ')  PG 09:00 JST            SPG 00:00 UTC
//!     extract(hour from tstz)     PG 9                    SPG 0
//!     extract(dow  from tstz)     PG 0 (Sunday)           SPG 6
//!     date_trunc('day', tstz)     PG 2020-01-02 00:00+09  SPG 2020-01-01 09:00+09
//!     tstz::date                  PG 2020-01-02           SPG 2020-01-01
//!     TIMESTAMP '…'::timestamptz  PG 00:00:00+09          SPG 09:00:00+09
//!     INSERT of a naive value     PG epoch 1577804400     SPG 1577836800
//!     current_date                PG 2026-07-27           SPG 2026-07-26
//!
//! The INSERT is the one that is not a rendering at all: the stored
//! INSTANT was nine hours from PG's, so a client that sets a session
//! zone — which JDBC and psycopg both do by default — wrote a different
//! moment than it read back.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

/// The named-zone lookups come from the host's zoneinfo, which the
/// server injects; round 221's tests established the shape, including
/// the honest self-skip when the host has no tzdata.
fn tz_engine() -> Engine {
    // A fixed clock, or the clock rewrite does not run at all and
    // `current_date` reports as an unknown function.
    // 2020-01-01 15:00:00Z — 2020-01-02 in Tokyo, so the local reading
    // and the UTC one name different DAYS.
    let mut e = Engine::new().with_clock(|| 1_577_890_800_000_000);
    e.set_tz_fns(
        spg_tzif::tz_offset_at,
        spg_tzif::tz_local_to_utc,
        spg_tzif::tz_canonical,
        spg_tzif::tz_abbrev_at,
    );
    e
}

fn host_has_tzdata() -> bool {
    spg_tzif::tz_offset_at("Asia/Tokyo", 0).is_some()
}

fn engine() -> Engine {
    let mut e = tz_engine();
    e.execute("SET TimeZone = 'Asia/Tokyo'").unwrap();
    e
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// Reading a field of an instant reads the LOCAL clock — which is the
/// whole reason the type exists.
#[test]
fn round523_extract_reads_the_session_clock() {
    if !host_has_tzdata() {
        return;
    }
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT extract(hour from TIMESTAMPTZ '2020-01-01 00:00:00Z')"
        ),
        "9"
    );
    // 2020-01-04 15:00Z is Sunday in Tokyo and Saturday in UTC, so a
    // report grouped by weekday put nine hours of every Sunday under
    // Saturday.
    assert_eq!(
        text(
            &mut e,
            "SELECT extract(dow from TIMESTAMPTZ '2020-01-04 15:00:00Z')"
        ),
        "0"
    );
    // Absolute fields do not shift.
    assert_eq!(
        text(
            &mut e,
            "SELECT extract(epoch from TIMESTAMPTZ '2020-01-01 00:00:00Z')"
        ),
        // PG renders an epoch with six fractional digits.
        "1577836800.000000"
    );
    // And a NAIVE timestamp has no zone to be read in.
    assert_eq!(
        text(
            &mut e,
            "SELECT extract(hour from TIMESTAMP '2020-01-01 00:00:00')"
        ),
        "0"
    );
}

/// A day boundary is a boundary of the LOCAL day. Truncating in UTC and
/// printing in the session zone did not even produce a midnight.
#[test]
fn round523_date_trunc_cuts_on_the_local_calendar() {
    if !host_has_tzdata() {
        return;
    }
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT date_trunc('day', TIMESTAMPTZ '2020-01-01 15:00:00Z')::text"
        ),
        "2020-01-02 00:00:00+09"
    );
    // A naive timestamp keeps truncating where it always did.
    assert_eq!(
        text(
            &mut e,
            "SELECT date_trunc('day', TIMESTAMP '2020-01-01 15:00:00')::text"
        ),
        "2020-01-01 00:00:00"
    );
}

/// Casting DOWN to a zone-free type reads the local clock — an
/// off-by-one-day for every instant in the last nine hours of a UTC day.
#[test]
fn round523_downcast_reads_the_local_clock() {
    if !host_has_tzdata() {
        return;
    }
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT (TIMESTAMPTZ '2020-01-01 15:00:00Z')::date::text"
        ),
        "2020-01-02"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT (TIMESTAMPTZ '2020-01-01 15:00:00Z')::timestamp::text"
        ),
        "2020-01-02 00:00:00"
    );
    // PG defines `x::timestamp` and `x AT TIME ZONE <session zone>` to
    // be the same value; they disagreed.
    assert_eq!(
        text(
            &mut e,
            "SELECT (TIMESTAMPTZ '2020-01-01 15:00:00Z')::timestamp \
             = (TIMESTAMPTZ '2020-01-01 15:00:00Z' AT TIME ZONE 'Asia/Tokyo')"
        ),
        "true"
    );
}

/// Casting UP from a zone-free type reads the naive value as a
/// wall-clock reading in the session zone.
#[test]
fn round523_upcast_reads_the_value_as_local() {
    if !host_has_tzdata() {
        return;
    }
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT (TIMESTAMP '2020-01-01 00:00:00')::timestamptz::text"
        ),
        "2020-01-01 00:00:00+09"
    );
    // An already-aware value is not shifted a second time.
    assert_eq!(
        text(
            &mut e,
            "SELECT (TIMESTAMPTZ '2020-01-01 00:00:00Z')::timestamptz::text"
        ),
        "2020-01-01 09:00:00+09"
    );
}

/// The stored INSTANT — not a rendering. Each shape is the one a client
/// actually writes.
#[test]
fn round523_insert_stores_the_instant_pg_stores() {
    if !host_has_tzdata() {
        return;
    }
    let mut e = engine();
    e.execute("CREATE TABLE tz1 (a timestamptz)").unwrap();
    for (values, expect) in [
        // A bare string: a wall-clock reading in the session zone.
        ("('2020-01-01 00:00:00')", "2020-01-01 00:00:00+09"),
        (
            "(TIMESTAMP '2020-01-01 00:00:00')",
            "2020-01-01 00:00:00+09",
        ),
        // An offset-less timestamptz literal is local too.
        (
            "(TIMESTAMPTZ '2020-01-01 00:00:00')",
            "2020-01-01 00:00:00+09",
        ),
        // These two already name an instant and must NOT shift — the
        // first cut of this fix moved the offset-bearing string twice.
        (
            "(TIMESTAMPTZ '2020-01-01 00:00:00Z')",
            "2020-01-01 09:00:00+09",
        ),
        ("('2020-01-01 00:00:00+05')", "2020-01-01 04:00:00+09"),
    ] {
        e.execute("DELETE FROM tz1").unwrap();
        e.execute(&format!("INSERT INTO tz1 VALUES {values}"))
            .unwrap();
        assert_eq!(
            text(&mut e, "SELECT a::text FROM tz1"),
            expect,
            "INSERT … VALUES {values}"
        );
    }
    // Naming the column takes the same path.
    e.execute("DELETE FROM tz1").unwrap();
    e.execute("INSERT INTO tz1 (a) VALUES ('2020-01-01 00:00:00')")
        .unwrap();
    assert_eq!(
        text(&mut e, "SELECT a::text FROM tz1"),
        "2020-01-01 00:00:00+09"
    );
}

/// `to_char` renders the local clock, and its zone tokens name the zone
/// the rest of the string is in.
#[test]
fn round523_to_char_renders_the_session_zone() {
    if !host_has_tzdata() {
        return;
    }
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT to_char(TIMESTAMPTZ '2020-01-01 00:00:00Z', 'YYYY-MM-DD HH24:MI TZ')"
        ),
        "2020-01-01 09:00 JST"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT to_char(TIMESTAMPTZ '2020-01-01 00:00:00Z', 'OF')"
        ),
        "+09"
    );
}

/// The local-clock family reads the session's wall clock; the
/// timestamptz spellings name an instant and do not move.
#[test]
fn round523_clock_family_reads_the_session_zone() {
    if !host_has_tzdata() {
        return;
    }
    let mut e = engine();
    // `current_date` is the local date, so it agrees with the local
    // reading of the current instant. Asserting the RELATION rather
    // than a literal keeps this true whenever it runs.
    assert_eq!(
        text(
            &mut e,
            "SELECT current_date = (current_timestamp AT TIME ZONE 'Asia/Tokyo')::date"
        ),
        "true"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT localtimestamp::date = (current_timestamp AT TIME ZONE 'Asia/Tokyo')::date"
        ),
        "true"
    );
}

/// A UTC session is unchanged — every shift above is a no-op there.
#[test]
fn round523_utc_session_is_untouched() {
    let mut e = tz_engine();
    e.execute("SET TimeZone = 'UTC'").unwrap();
    assert_eq!(
        text(
            &mut e,
            "SELECT extract(hour from TIMESTAMPTZ '2020-01-01 00:00:00Z'), \
             (TIMESTAMP '2020-01-01 00:00:00')::timestamptz::text, \
             date_trunc('day', TIMESTAMPTZ '2020-01-01 15:00:00Z')::text"
        ),
        "0|2020-01-01 00:00:00+00|2020-01-01 00:00:00+00"
    );
}
