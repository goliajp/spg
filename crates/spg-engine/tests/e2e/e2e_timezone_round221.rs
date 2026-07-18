//! v7.39 (round 221) — named IANA timezone support in AT TIME ZONE /
//! date_trunc / make_timestamptz, plus timetz conversions and the
//! SQL-standard `TIME WITH TIME ZONE` literal spelling. The tz
//! infrastructure (spg-tzif + the engine's injected TzOffsetFn family)
//! existed for SET TIME ZONE / rendering; these call sites simply never
//! consulted it. Live-PG18.4 differential (2026-07-19), all values now
//! byte-equal:
//!   timestamp '2026-07-19 12:00' AT TIME ZONE 'Asia/Tokyo' → 03:00+00
//!   timestamptz '… 12:00+00' AT TIME ZONE 'Asia/Tokyo'     → 21:00
//!   make_timestamptz(2026,7,19,12,0,0,'Asia/Tokyo')        → 03:00+00
//!   date_trunc('week', tstz, 'Asia/Tokyo')                 → 07-12 15:00+00
//!   timetz '12:00:00+05' AT TIME ZONE 'UTC'                → 07:00:00+00
//! Tests self-skip when the host has no zoneinfo (spg-tzif degrades
//! honestly), so CI without tzdata stays green.

use spg_engine::{Engine, QueryResult};

fn tz_engine() -> Engine {
    let mut e = Engine::new();
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

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => format!("{:?}", rows[0].values[0]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn at_time_zone_named_iana() {
    if !host_has_tzdata() {
        return; // honest degrade: no zoneinfo on this host
    }
    let mut e = tz_engine();
    // naive → tstz: 12:00 Tokyo = 03:00 UTC (1784430000000000 µs).
    assert_eq!(
        one(
            &mut e,
            "SELECT timestamp '2026-07-19 12:00:00' AT TIME ZONE 'Asia/Tokyo'"
        ),
        "Timestamp(1784430000000000)"
    );
    // DST-aware: New York in January is EST (-5): 12:00 → 17:00 UTC.
    assert_eq!(
        one(
            &mut e,
            "SELECT timestamp '2026-01-15 12:00:00' AT TIME ZONE 'America/New_York'"
        ),
        "Timestamp(1768496400000000)"
    );
}

#[test]
fn make_timestamptz_with_zone() {
    if !host_has_tzdata() {
        return;
    }
    let mut e = tz_engine();
    assert_eq!(
        one(
            &mut e,
            "SELECT make_timestamptz(2026, 7, 19, 12, 0, 0, 'Asia/Tokyo')"
        ),
        "Timestamp(1784430000000000)"
    );
}

#[test]
fn date_trunc_with_zone() {
    if !host_has_tzdata() {
        return;
    }
    let mut e = tz_engine();
    // Week boundary in Tokyo local time: Mon 2026-07-13 00:00 JST
    // = 2026-07-12 15:00 UTC.
    assert_eq!(
        one(
            &mut e,
            "SELECT date_trunc('week', timestamp '2026-07-19 12:00:00', 'Asia/Tokyo')"
        ),
        "Timestamp(1783868400000000)"
    );
}

#[test]
fn timetz_literal_and_at_time_zone() {
    // Fixed offsets — no tzdata needed.
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT time with time zone '12:00:00+05'"),
        "TimeTz { us: 43200000000, offset_secs: 18000 }"
    );
    // Same instant re-anchored to UTC: 12:00+05 = 07:00+00.
    assert_eq!(
        one(&mut e, "SELECT timetz '12:00:00+05' AT TIME ZONE 'UTC'"),
        "TimeTz { us: 25200000000, offset_secs: 0 }"
    );
    // The long spelling parses for timestamp too.
    assert_eq!(
        one(
            &mut e,
            "SELECT timestamp without time zone '2026-07-19 00:00:00'"
        ),
        "Timestamp(1784419200000000)"
    );
}

#[test]
fn unknown_zone_still_errors() {
    // No tz fns installed → named zone errors honestly (not silently UTC).
    let mut e = Engine::new();
    let err = e
        .execute("SELECT timestamp '2026-07-19 12:00:00' AT TIME ZONE 'Asia/Tokyo'")
        .unwrap_err()
        .to_string();
    assert!(err.contains("not recognized"), "{err}");
}
