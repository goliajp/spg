//! v7.39 (round 289) — casting a zone-bearing literal to a zone-less
//! type drops the zone; it does not convert.
//!
//! `'2020-01-01 10:00:00+02'::timestamp` is `10:00:00` in PG. SPG
//! answered `08:00:00` — it normalised to UTC, the way a timestamptz
//! literal is normalised, and returned a DIFFERENT INSTANT with no
//! error. That is the silent-wrong shape: no exception, no warning,
//! just a value two hours off.
//!
//! The parse itself was already right; what it reports is the local
//! clock and the offset SEPARATELY, and the `::timestamp` path was
//! applying the offset it should have ignored.
//!
//! Every expectation was read off live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    spg_engine::eval::value_to_text(&rows[0].values[0])
}

#[test]
fn a_numeric_offset_is_ignored_not_applied() {
    let mut e = Engine::new();
    // The bug: these all answered the UTC-normalised instant.
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00+02'::timestamp"),
        "2020-01-01 10:00:00",
    );
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00-05:30'::timestamp"),
        "2020-01-01 10:00:00",
    );
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00 UTC'::timestamp"),
        "2020-01-01 10:00:00",
    );
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00Z'::timestamp"),
        "2020-01-01 10:00:00",
    );
}

#[test]
fn a_naive_literal_is_unchanged() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00'::timestamp"),
        "2020-01-01 10:00:00",
    );
    // Seconds-optional and date-only forms keep working.
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:30'::timestamp"),
        "2020-01-01 10:30:00",
    );
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01'::timestamp"),
        "2020-01-01 00:00:00",
    );
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00.123456+02'::timestamp"),
        "2020-01-01 10:00:00.123456",
    );
}

/// `Engine::new()` carries no timezone lookups — the no_std engine
/// takes them from its host. Without them a NAMED zone cannot resolve,
/// so a pin that uses one has to inject them exactly as the server and
/// spg-embedded do.
fn engine_with_zones() -> Engine {
    let mut e = Engine::new();
    e.set_tz_fns(
        spg_tzif::tz_offset_at,
        spg_tzif::tz_local_to_utc,
        spg_tzif::tz_canonical,
        spg_tzif::tz_abbrev_at,
    );
    e
}

#[test]
fn timestamptz_still_normalises_to_utc() {
    // The other half: a zone-BEARING target must still convert. Fixing
    // the zone-less cast by ignoring offsets everywhere would have
    // broken this, and the instants below are the witness.
    let mut e = engine_with_zones();
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00+02'::timestamptz"),
        "2020-01-01 08:00:00",
    );
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00 America/New_York'::timestamptz"),
        "2020-01-01 15:00:00",
    );
    // July is EDT, not EST — the zone database is consulted per instant.
    assert_eq!(
        one(&mut e, "SELECT '2020-07-01 10:00:00 America/New_York'::timestamptz"),
        "2020-07-01 14:00:00",
    );
}

#[test]
fn the_era_and_sentinel_spellings_survive_the_refactor() {
    // The first attempt at the wall-clock reader duplicated the parse
    // body and silently lost `BC`; the two readers share one parts
    // extractor now, and these are what caught it.
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT '0044-03-15 10:20:30 BC'::timestamp"),
        "0044-03-15 10:20:30 BC",
    );
    assert_eq!(
        one(&mut e, "SELECT 'epoch'::timestamp"),
        "1970-01-01 00:00:00",
    );
    assert_eq!(one(&mut e, "SELECT 'infinity'::timestamp"), "infinity");
    assert_eq!(one(&mut e, "SELECT '-infinity'::timestamp"), "-infinity");
}

#[test]
fn a_malformed_trailer_is_still_a_parse_error() {
    // Ignoring the zone must not become "ignore whatever trails".
    let mut e = Engine::new();
    assert!(e.execute("SELECT '2020-01-01 10:00:00 xyz'::timestamp").is_err());
    assert!(e.execute("SELECT 'not a timestamp'::timestamp").is_err());
}
