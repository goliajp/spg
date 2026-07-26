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
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
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
        text_of(
            &mut e,
            "SELECT (TIMESTAMPTZ '2024-01-15 12:00:00+00')::text"
        ),
        "2024-01-15 07:00:00-05"
    );
    assert_eq!(
        text_of(
            &mut e,
            "SELECT (TIMESTAMPTZ '2024-07-15 12:00:00+00')::text"
        ),
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
        text_of(
            &mut e,
            "SELECT (TIMESTAMPTZ '2024-07-15 12:00 Asia/Tokyo')::text"
        ),
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

/// v7.39 (round 502) — the timezone catalogues are listable.
///
/// SPG resolved named zones correctly — the DST pins above measure that
/// against PG18 — but `pg_timezone_names` answered "relation does not
/// exist", so a client populating a timezone picker had nothing to read.
///
/// The rows are PG's shape and PG's values; what these pin is the part
/// that took a measurement to get right. Listing every file under the
/// zoneinfo tree gives 1794 names where PG18 gives 487, because the tree
/// also holds the `posix/` and `right/` re-encodings and tzdata's
/// backward-compatibility symlinks. Those stay RESOLVABLE and are simply
/// not listed, which is PG's own split.
#[test]
fn round502_timezone_catalogues_list_canonical_zones() {
    let mut e = engine();
    e.set_tz_all_fn(spg_tzif::tz_all_at);

    let n: i64 = text_of(&mut e, "SELECT count(*) FROM pg_timezone_names")
        .parse()
        .unwrap();
    // A host with no tzdata legitimately has none; anything else must be
    // the canonical set, not the whole tree.
    if n == 0 {
        return;
    }
    assert!(
        (300..900).contains(&n),
        "canonical zone count {n} — the posix/ and right/ subtrees or the \
         backward-compat symlinks are being listed (PG18: 487)"
    );
    for bad in ["posix/UTC", "right/UTC"] {
        assert_eq!(
            text_of(&mut e, &format!("SELECT count(*) FROM pg_timezone_names WHERE name = '{bad}'")),
            "0",
            "{bad} should not be listed"
        );
    }
    // A deprecated alias still resolves. Whether it is also LISTED is
    // host-dependent and deliberately not asserted: tzdata ships the
    // backward names as hard links on both hosts checked here, so the
    // symlink filter does not remove them, and PG's 487 is its own
    // curated list rather than a property of the directory. Asserting
    // absence here would pin the host, not the behaviour.
    e.execute("SET TIME ZONE 'Asia/Calcutta'").unwrap();
    assert_eq!(text_of(&mut e, "SHOW TimeZone"), "Asia/Calcutta");

    // Values match PG18 for zones whose offset does not move.
    assert_eq!(
        text_of(&mut e, "SELECT abbrev FROM pg_timezone_names WHERE name = 'Asia/Tokyo'"),
        "JST"
    );
    assert_eq!(
        text_of(&mut e, "SELECT is_dst::text FROM pg_timezone_names WHERE name = 'Asia/Tokyo'"),
        "false"
    );
    // And the abbreviation view is keyed by designation, deduplicated.
    let a: i64 = text_of(&mut e, "SELECT count(*) FROM pg_timezone_abbrevs")
        .parse()
        .unwrap();
    assert!(a > 0 && a < n, "abbrevs {a} should dedup below names {n}");
}
