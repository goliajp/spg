//! read01 round 327 (V44) — `array_agg` keeps the zone identity.
//!
//! SPG carries a `timestamptz` as `Value::Timestamp` at runtime (the
//! instant, in UTC micros), so the array `array_agg` builds out of the
//! accumulated values is a `TimestampArray` — and `pg_typeof` answered
//! `timestamp without time zone[]` for `array_agg(timestamptz_col)`, with
//! the rendered text losing the `+00` suffix that says which type it is.
//!
//! The static type already knew better: `infer_agg_type` has mapped
//! Timestamptz ⇒ TimestamptzArray all along. It is the finalized VALUE
//! that had to be re-tagged to agree with it.
//!
//! This is the third code path in one family — round 289 fixed the array
//! constructor (V31), round 326 the literal cast (V43), and both the
//! grouped and the WINDOW aggregate needed it here.
//!
//! Measured on PG 18.4:
//!   * `pg_typeof(array_agg(a))` → `timestamp with time zone[]`
//!   * `array_agg(a)::text` → `{"2020-01-01 06:00:00+00"}`
//!   * the same for `array_agg(a) OVER ()`

use spg_engine::Engine;
use spg_storage::Value;

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        spg_engine::QueryResult::Rows { rows, .. } => match rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
        {
            Some(Value::Text(t)) => t.to_string(),
            other => panic!("`{sql}` did not return text: {other:?}"),
        },
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a TIMESTAMPTZ, b TIMESTAMP, c DATE, d INTERVAL)")
        .unwrap();
    e.execute(
        "INSERT INTO t VALUES ('2020-01-01 08:00:00+02', '2020-01-01 08:00:00', \
         '2020-01-01', '1 day')",
    )
    .unwrap();
    e
}

#[test]
fn array_agg_of_a_timestamptz_is_a_timestamptz_array() {
    let mut e = fixture();
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(array_agg(a)) FROM t"),
        "timestamp with time zone[]",
    );
    assert_eq!(
        text(&mut e, "SELECT array_agg(a)::text FROM t"),
        "{\"2020-01-01 06:00:00+00\"}",
        "the rendered element carries the offset, which is what names the type"
    );
}

/// The zone-less sibling must not drift the other way.
#[test]
fn array_agg_of_a_timestamp_stays_zone_less() {
    let mut e = fixture();
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(array_agg(b)) FROM t"),
        "timestamp without time zone[]",
    );
    assert_eq!(
        text(&mut e, "SELECT array_agg(b)::text FROM t"),
        "{\"2020-01-01 08:00:00\"}",
    );
}

/// A window aggregate takes a different code path and lost it too.
#[test]
fn a_window_array_agg_keeps_the_zone_identity() {
    let mut e = fixture();
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(array_agg(a) OVER ()) FROM t"),
        "timestamp with time zone[]",
    );
    assert_eq!(
        text(&mut e, "SELECT (array_agg(a) OVER ())::text FROM t"),
        "{\"2020-01-01 06:00:00+00\"}",
    );
}

/// The neighbouring element types are unaffected.
#[test]
fn the_other_temporal_element_types_are_unchanged() {
    let mut e = fixture();
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(array_agg(c)) FROM t"),
        "date[]"
    );
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(array_agg(d)) FROM t"),
        "interval[]",
    );
}

/// And a GROUP BY keeps it as well.
#[test]
fn a_grouped_array_agg_keeps_the_zone_identity() {
    let mut e = fixture();
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(array_agg(a)) FROM t GROUP BY a"),
        "timestamp with time zone[]",
    );
}
