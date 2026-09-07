//! v7.40.11 — `timestamptz + interval '1 day'` was 24 hours, not a
//! calendar day.
//!
//! Reported against 7.40.9 (§3.19). In a session zone with daylight
//! saving, adding a day-or-larger interval to a `timestamptz` gives an
//! answer an hour off — twice a year, silently, in the direction that
//! depends on which edge you cross.
//!
//! Every number below is measured on the PostgreSQL 18.6 oracle with
//! `SET TimeZone = 'America/Denver'` (springs forward 2026-03-08 02:00,
//! falls back 2026-11-01 02:00), as `extract(epoch …)` — the INSTANT,
//! not its rendering, because the instant is what the arithmetic
//! decides and what a retention window compares.
//!
//! From `'2026-03-07 12:00-07'` (epoch 1772910000):
//!
//! ```text
//!                        PG 18.6 epoch   delta from the base
//!   + interval '1 day'    1772992800     +82800   = 23 h   ← a calendar day
//!   + interval '24 hours' 1772996400     +86400   = 24 h   ← a duration
//! ```
//!
//! That pair is the whole finding: PostgreSQL disagrees with ITSELF
//! between the two, which is correct, and is the entire distinction
//! between a duration and a calendar step. SPG gave +86400 for both.
//!
//! Across the November edge the day is 25 hours, in the other
//! direction — from `'2026-10-31 12:00-06'` (1793469600),
//! `+ interval '1 day'` is 1793559600, +90000.
//!
//! The rule PostgreSQL applies: for a `timestamptz`, the MONTHS and
//! DAYS fields of an interval are calendar steps taken in the session
//! zone — convert to local wall clock, step the calendar, convert back
//! — and only the time part is an absolute duration. A `timestamp`
//! without a zone has no zone to step in, so it keeps the naive
//! arithmetic; the rows that already agreed are the ones where that
//! distinction does not arise.
//!
//! Anyone deploying with a local session zone — the ordinary thing to
//! do — got daily aggregates, "same time tomorrow" schedules and
//! retention windows an hour wrong on two days a year. It is the
//! failure mode that gets diagnosed as a leap-second bug for a week.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

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

/// The INSTANT an expression names, in epoch seconds. Renderer-free:
/// `Value::Timestamp` carries no zone, so comparing rendered text would
/// be testing the printer rather than the arithmetic.
fn epoch(e: &mut Engine, expr: &str) -> i64 {
    let sql = format!("SELECT extract(epoch FROM {expr})::bigint");
    match e.execute(&sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            Value::BigInt(n) => n,
            Value::Int(n) => i64::from(n),
            ref o => panic!("{sql}: {o:?}"),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

fn text_of(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The reported rows, in the zone they were reported in, as instants.
#[test]
fn a_calendar_step_crosses_a_dst_edge_the_way_pg_does() {
    let mut e = engine();
    e.execute("SET TimeZone = 'America/Denver'")
        .expect("set tz");
    // (expression, PG 18.6's epoch)
    let cases: &[(&str, i64)] = &[
        ("'2026-03-07 12:00-07'::timestamptz", 1_772_910_000),
        (
            "'2026-03-07 12:00-07'::timestamptz + interval '1 day'",
            1_772_992_800,
        ),
        (
            "'2026-03-07 12:00-07'::timestamptz + interval '24 hours'",
            1_772_996_400,
        ),
        (
            "'2026-10-31 12:00-06'::timestamptz + interval '1 day'",
            1_793_559_600,
        ),
        (
            "'2026-03-07 12:00-07'::timestamptz + interval '1 month'",
            1_775_584_800,
        ),
        (
            "'2026-03-07 12:00-07'::timestamptz + interval '30 days'",
            1_775_498_400,
        ),
        (
            "'2026-03-07 12:00-07'::timestamptz + interval '1 week'",
            1_773_511_200,
        ),
        (
            "'2026-03-07 12:00-07'::timestamptz + interval '1 day 2 hours'",
            1_773_000_000,
        ),
    ];
    for (expr, want) in cases {
        assert_eq!(epoch(&mut e, expr), *want, "{expr}");
    }
}

/// The pair that names the rule, stated as the deltas rather than the
/// absolute instants, so the sentence is legible in the failure.
#[test]
fn a_day_is_twenty_three_hours_across_the_spring_edge() {
    let mut e = engine();
    e.execute("SET TimeZone = 'America/Denver'")
        .expect("set tz");
    let base = epoch(&mut e, "'2026-03-07 12:00-07'::timestamptz");
    assert_eq!(
        epoch(
            &mut e,
            "'2026-03-07 12:00-07'::timestamptz + interval '1 day'"
        ) - base,
        23 * 3600,
        "a calendar day across the spring-forward edge is 23 hours"
    );
    assert_eq!(
        epoch(
            &mut e,
            "'2026-03-07 12:00-07'::timestamptz + interval '24 hours'"
        ) - base,
        24 * 3600,
        "and a duration of 24 hours is 24 hours"
    );
    let fall = epoch(&mut e, "'2026-10-31 12:00-06'::timestamptz");
    assert_eq!(
        epoch(
            &mut e,
            "'2026-10-31 12:00-06'::timestamptz + interval '1 day'"
        ) - fall,
        25 * 3600,
        "and across the fall-back edge it is 25 hours"
    );
}

/// Subtraction across the edge is the mirror of addition: stepping a
/// day forward and a day back must return to where it started.
#[test]
fn subtraction_across_an_edge_is_the_mirror() {
    let mut e = engine();
    e.execute("SET TimeZone = 'America/Denver'")
        .expect("set tz");
    for (expr, want) in [
        (
            "'2026-03-08 12:00-06'::timestamptz - interval '1 day'",
            1_772_910_000_i64,
        ),
        (
            "'2026-11-01 12:00-07'::timestamptz - interval '1 day'",
            1_793_469_600,
        ),
        (
            "'2026-03-07 12:00-07'::timestamptz - interval '1 day'",
            1_772_823_600,
        ),
    ] {
        assert_eq!(epoch(&mut e, expr), want, "{expr}");
    }
}

/// The half that must not move: a year that lands on the same side of
/// the edge, a `date`, and a `timestamp` with no zone to step in.
#[test]
fn the_rows_that_already_agreed_still_do() {
    let mut e = engine();
    e.execute("SET TimeZone = 'America/Denver'")
        .expect("set tz");
    assert_eq!(
        epoch(
            &mut e,
            "'2026-03-07 12:00-07'::timestamptz + interval '1 year'"
        ),
        1_804_446_000
    );
    // A naive timestamp has no zone to step in, so it keeps the naive
    // arithmetic — the wall clock moves by exactly one day.
    assert_eq!(
        text_of(
            &mut e,
            "SELECT ('2026-03-07 12:00'::timestamp + interval '1 day')::text"
        ),
        "2026-03-08 12:00:00"
    );
    assert_eq!(
        text_of(
            &mut e,
            "SELECT ('2026-03-07'::date + interval '1 day')::text"
        ),
        "2026-03-08 00:00:00"
    );
}

/// In UTC nothing steps, which is why the reporter's own deployment was
/// safe and why nothing here had caught it: the shipped images default
/// to UTC and so do PostgreSQL's.
#[test]
fn utc_is_unchanged() {
    let mut e = engine();
    e.execute("SET TimeZone = 'UTC'").expect("set tz");
    let base = epoch(&mut e, "'2026-03-07 12:00+00'::timestamptz");
    assert_eq!(base, 1_772_884_800);
    for iv in ["1 day", "24 hours"] {
        assert_eq!(
            epoch(
                &mut e,
                &format!("'2026-03-07 12:00+00'::timestamptz + interval '{iv}'")
            ),
            1_772_971_200,
            "{iv} in UTC"
        );
    }
}

/// A COLUMN, not just a cast literal — the reporter's retention query
/// is `WHERE occurred_at < now() - interval '30 days'` over a
/// timestamptz column, so the type has to be known there too.
#[test]
fn a_timestamptz_column_steps_the_calendar_too() {
    let mut e = engine();
    e.execute("SET TimeZone = 'America/Denver'")
        .expect("set tz");
    e.execute("CREATE TABLE tz1 (t TIMESTAMPTZ)").expect("ddl");
    e.execute("INSERT INTO tz1 VALUES ('2026-03-07 12:00-07')")
        .expect("insert");
    assert_eq!(
        epoch(&mut e, "(SELECT t FROM tz1) + interval '1 day'"),
        1_772_992_800,
        "a column's own type decides this, not the literal's spelling"
    );
    assert_eq!(
        epoch(&mut e, "(SELECT t FROM tz1) + interval '24 hours'"),
        1_772_996_400
    );
    // And through the ordinary projection, which is the shape a
    // retention window is written in.
    let got = match e
        .execute("SELECT extract(epoch FROM t + interval '1 day')::bigint FROM tz1")
        .expect("projection")
    {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(got, Value::BigInt(1_772_992_800));
}
