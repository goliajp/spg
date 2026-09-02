//! v7.39.12 — a scalar subquery keeps the type its inner SELECT
//! declared, and the instant it stores does not move.
//!
//! Reported by sentori against 7.39.11. Measured on PostgreSQL 18.6
//! before anything was changed:
//!
//! ```text
//!                                     PG 18.6                  SPG 7.39.11
//!   pg_typeof over timestamptz   timestamp with time zone   WITHOUT time zone
//!   pg_typeof over text          text                       unknown
//!   pg_typeof over bigint[]      bigint[]                   integer[]
//!   pg_typeof over jsonb         jsonb                      unknown
//! ```
//!
//! The first is not an introspection complaint. Assign one into a
//! `timestamptz` column under a session whose `TimeZone` is not UTC —
//! which is what a "recompute `first_seen` from `min(occurred_at)`"
//! statement does — and the stored instant moves by the session's
//! offset, because the assignment coerces a `timestamp` instead of
//! moving a `timestamptz`:
//!
//! ```text
//!   SET TimeZone = 'Asia/Tokyo';
//!   UPDATE iss SET first_seen = (SELECT min(occurred_at) FROM ev);
//!   extract(epoch FROM first_seen)
//!
//!     PG 18.6      1767225600
//!     SPG 7.39.11  1767193200      <- nine hours, the session's offset
//! ```
//!
//! Under UTC both agree, which is why nothing had caught it: the
//! published image defaults to UTC and so do the official ones.
//!
//! A scalar subquery materialises through a literal expression, and
//! `substitute.rs` already records three members of this same class —
//! BIGINT narrowing to integer, `CHAR(8)` truncating to one character,
//! `BIT(n)` failing outright. SPG carries no scalar `Value::Timestamptz`
//! — the zone lives in the column's declared type — so the value alone
//! could not preserve it; the declared type is passed in now.

use spg_engine::{Engine, QueryResult};

fn scalar(e: &mut Engine, sql: &str) -> String {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    spg_engine::eval::value_to_text(&rows[0].values[0])
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE ev (issue_id int, occurred_at timestamptz)",
        "INSERT INTO ev VALUES (1, '2026-01-01 00:00:00+00')",
        "CREATE TABLE iss (id int, first_seen timestamptz)",
        "INSERT INTO iss VALUES (1, NULL)",
    ] {
        e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"));
    }
    e
}

#[test]
fn the_stored_instant_does_not_move_with_the_session_zone() {
    // The one that is not about introspection.
    let mut e = seeded();
    e.execute("SET TimeZone = 'Asia/Tokyo'").unwrap();
    assert_eq!(
        scalar(&mut e, "SHOW TimeZone"),
        "Asia/Tokyo",
        "the control: without the SET this test proves nothing"
    );
    e.execute("UPDATE iss SET first_seen = (SELECT min(occurred_at) FROM ev)")
        .unwrap();
    assert_eq!(
        scalar(
            &mut e,
            "SELECT extract(epoch FROM first_seen)::bigint FROM iss"
        ),
        "1767225600",
        "PostgreSQL 18.6 stores 1767225600; 1767193200 is that instant moved \
         by the session's nine-hour offset"
    );
}

#[test]
fn under_utc_it_was_already_right_and_still_is() {
    // Why nobody had caught it: the published image defaults to UTC.
    let mut e = seeded();
    e.execute("UPDATE iss SET first_seen = (SELECT min(occurred_at) FROM ev)")
        .unwrap();
    assert_eq!(
        scalar(
            &mut e,
            "SELECT extract(epoch FROM first_seen)::bigint FROM iss"
        ),
        "1767225600"
    );
}

#[test]
fn a_scalar_subquery_keeps_its_declared_type() {
    let mut e = seeded();
    assert_eq!(
        scalar(
            &mut e,
            "SELECT pg_typeof((SELECT min(occurred_at) FROM ev))::text"
        ),
        "timestamp with time zone"
    );
    assert_eq!(
        scalar(&mut e, "SELECT pg_typeof((SELECT 'x'::text))::text"),
        "text",
        "a bare string literal is `unknown` until something types it"
    );
    assert_eq!(
        scalar(
            &mut e,
            "SELECT pg_typeof((SELECT ARRAY[1]::bigint[]))::text"
        ),
        "bigint[]",
        "an array of small integers rebuilds as integer[] unless told otherwise"
    );
}

#[test]
fn the_types_that_already_survived_still_do() {
    // sentori's control list: uuid, numeric, date and plain timestamp
    // came through correctly before this change and must still.
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "SELECT pg_typeof((SELECT '2026-01-01'::date))::text",
            "date",
        ),
        (
            "SELECT pg_typeof((SELECT '2026-01-01 00:00'::timestamp))::text",
            "timestamp without time zone",
        ),
        ("SELECT pg_typeof((SELECT 1.5::numeric))::text", "numeric"),
        (
            "SELECT pg_typeof((SELECT count(*) FROM (SELECT 1) x))::text",
            "bigint",
        ),
    ] {
        assert_eq!(scalar(&mut e, sql), want, "{sql}");
    }
}
