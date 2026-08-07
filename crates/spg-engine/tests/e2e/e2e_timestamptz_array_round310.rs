//! v7.39 (round 310, V31) — a timestamptz array keeps its identity.
//!
//! `ARRAY['…+02'::timestamptz]` came back typed `timestamp without time
//! zone[]`, rendered without its offset, and was refused by a
//! `timestamptz[]` column. `Value::Timestamp` is the runtime form of
//! BOTH timestamp types — the zone-ness rides on the static type, the
//! way enum-ness and composite-ness do (rounds 54 / 56) — so an array
//! builder that picks its variant from what it materialised could only
//! ever answer the zone-less type.
//!
//! Fixing the label alone would have been worse than leaving it. The
//! INSERT that used to be REFUSED then succeeded and stored the wrong
//! instant: the literal-folding path INSERT VALUES uses reaches
//! `cast_value` directly, where `::timestamp` and `::timestamptz` shared
//! one arm that discards the literal's offset. That is a pre-existing
//! silent wrong value on its own — `'…10:00:00+02'::timestamptz` into a
//! timestamptz column stored 10:00 where PG stores 08:00 — and it is
//! fixed here because this round would otherwise have spread it to
//! arrays.
//!
//! Every expectation read off live PG 18.4 (2026-07-21), session UTC.

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

#[test]
fn an_array_of_timestamptz_keeps_the_type_and_the_offset() {
    let mut e = engine();
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_typeof(ARRAY['2020-01-01 10:00:00+02'::timestamptz])"
        ),
        "timestamp with time zone[]"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT ARRAY['2020-01-01 10:00:00+02'::timestamptz]"
        ),
        "{\"2020-01-01 08:00:00+00\"}"
    );
    // Two elements, both converted and both carrying the offset.
    assert_eq!(
        one(
            &mut e,
            "SELECT ARRAY['2020-01-01 10:00:00+02'::timestamptz, \
             '2020-06-01 10:00:00+02'::timestamptz]"
        ),
        "{\"2020-01-01 08:00:00+00\",\"2020-06-01 08:00:00+00\"}"
    );
}

/// The zone-LESS array must be untouched. Upgrading too eagerly would
/// just move the wrong answer to the other type.
#[test]
fn a_plain_timestamp_array_is_left_alone() {
    let mut e = engine();
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_typeof(ARRAY['2020-01-01 10:00:00'::timestamp])"
        ),
        "timestamp without time zone[]"
    );
    assert_eq!(
        one(&mut e, "SELECT ARRAY['2020-01-01 10:00:00'::timestamp]"),
        "{\"2020-01-01 10:00:00\"}"
    );
    // A mixed constructor resolves to the plain type, and stays plain.
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_typeof(ARRAY['2020-01-01 10:00:00'::timestamp, \
             '2020-06-01 10:00:00+02'::timestamptz])"
        ),
        "timestamp without time zone[]"
    );
}

/// The half the ledger recorded: the column used to refuse the array.
#[test]
fn a_timestamptz_array_column_accepts_and_returns_it() {
    let mut e = engine();
    e.execute("CREATE TABLE ta (id int, ts timestamptz[])")
        .unwrap();
    e.execute("INSERT INTO ta VALUES (1, ARRAY['2020-01-01 10:00:00+02'::timestamptz])")
        .unwrap();
    assert_eq!(
        one(&mut e, "SELECT ts FROM ta"),
        "{\"2020-01-01 08:00:00+00\"}"
    );
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(ts) FROM ta"),
        "timestamp with time zone[]"
    );
}

/// The pre-existing silent wrong value this round had to fix before the
/// array work could be correct: INSERT VALUES folds its literals through
/// a path that reaches `cast_value` directly, and `::timestamptz` there
/// was discarding the offset instead of applying it. PG stores 08:00.
#[test]
fn an_inserted_timestamptz_literal_is_converted_not_truncated() {
    let mut e = engine();
    e.execute("CREATE TABLE ts (id int, t timestamptz)")
        .unwrap();
    e.execute("INSERT INTO ts VALUES (1, '2020-01-01 10:00:00+02'::timestamptz)")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT t FROM ts"), "2020-01-01 08:00:00");
    // The zone-less column keeps round 289's rule — the offset is
    // discarded there, and that must not have moved.
    e.execute("CREATE TABLE tw (id int, t timestamp)").unwrap();
    e.execute("INSERT INTO tw VALUES (1, '2020-01-01 10:00:00+02'::timestamp)")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT t FROM tw"), "2020-01-01 10:00:00");
}

/// A naive literal has no offset to apply, so both types agree on it —
/// which is what the context-aware arm already assumed when it fell
/// through to the shared cast.
#[test]
fn a_naive_literal_reads_the_same_either_way() {
    let mut e = engine();
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00'::timestamptz"),
        "2020-01-01 10:00:00"
    );
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 10:00:00'::timestamp"),
        "2020-01-01 10:00:00"
    );
}
