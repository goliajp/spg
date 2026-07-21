//! read01 round 326 (V43) — casting a string literal to a temporal array.
//!
//! `'{1,2}'::int[]` worked; `'{"2020-01-01 08:00:00"}'::timestamp[]`,
//! `::timestamptz[]` and `::interval[]` all failed with a plain type
//! mismatch — an entire literal form that did not exist for the temporal
//! arrays, because only `Value::TextArray` (an `ARRAY[…]` constructor) had
//! a path into the typed-array coercion, never a `Value::Text` literal.
//!
//! The error also named the wrong target: the parser widened BOTH
//! `::timestamp[]` and `::timestamptz[]` to `timestamptz_array`, so a
//! zone-less cast reported `TIMESTAMPTZ[]` and lost its identity on the
//! way — the V31 / V44 family, in the cast路径.
//!
//! Expectations from live PG 18.4:
//!   * `'{"2020-01-01 08:00:00"}'::timestamp[]` → `{"2020-01-01 08:00:00"}`
//!   * `'{"2020-01-01 08:00:00+02"}'::timestamptz[]` →
//!     `{"2020-01-01 06:00:00+00"}` (the offset is applied)
//!   * `pg_typeof` → `timestamp without time zone[]` /
//!     `timestamp with time zone[]`
//!   * `'{1 day,2 hours}'::interval[]` → `{"1 day",02:00:00}`

use spg_engine::Engine;
use spg_storage::Value;

fn scalar(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        spg_engine::QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
            .unwrap_or_else(|| panic!("no cell for `{sql}`")),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn text(e: &mut Engine, sql: &str) -> String {
    match scalar(e, sql) {
        Value::Text(t) => t.to_string(),
        other => panic!("`{sql}` did not return text: {other:?}"),
    }
}

/// The literal form exists now, and keeps the wall clock for the
/// zone-less type.
#[test]
fn a_timestamp_array_literal_casts() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT ('{\"2020-01-01 08:00:00\"}'::timestamp[])::text"
        ),
        "{\"2020-01-01 08:00:00\"}",
    );
}

/// …and the two temporal array types stay distinct, which is what the
/// shared `timestamptz_array` widening destroyed.
#[test]
fn the_two_temporal_array_types_keep_their_identity() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT pg_typeof('{\"2020-01-01\"}'::timestamp[])"),
        "timestamp without time zone[]",
    );
    assert_eq!(
        text(&mut e, "SELECT pg_typeof('{\"2020-01-01\"}'::timestamptz[])"),
        "timestamp with time zone[]",
    );
}

/// `::interval[]` was in the same hole.
#[test]
fn an_interval_array_literal_casts() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT ('{1 day,2 hours}'::interval[])::text"),
        "{\"1 day\",02:00:00}",
    );
}

/// The forms that already worked keep working.
#[test]
fn the_working_array_casts_are_untouched() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT ('{1,2}'::int[])::text"),
        "{1,2}"
    );
    assert_eq!(
        text(&mut e, "SELECT ('{2020-01-01,2020-01-02}'::date[])::text"),
        "{2020-01-01,2020-01-02}",
    );
    assert_eq!(
        text(&mut e, "SELECT pg_typeof('{2020-01-01}'::date[])"),
        "date[]",
    );
}

/// A malformed literal still reports PG's array error (round 325), not a
/// bare type mismatch.
#[test]
fn a_malformed_temporal_array_literal_says_so() {
    let mut e = Engine::new();
    let msg = match e.execute("SELECT 'abc'::timestamp[]") {
        Ok(v) => panic!("expected an error, got {v:?}"),
        Err(x) => format!("{x}"),
    };
    assert!(
        msg.contains("malformed array literal: \"abc\""),
        "expected PG's array error, got {msg}"
    );
}
