//! v7.40.9 — a `timestamptz` keeps its offset through every JSON
//! builder, not only the one-argument scalar.
//!
//! Reported against 7.40.7. The customer's CLI dumps and exports every
//! table with `SELECT row_to_json(t) FROM <table> t`, so a dump taken
//! from SPG carried no timezone on any timestamp and one taken from
//! PostgreSQL did. They found it by restoring the same file into both
//! and diffing that statement's output table by table: every table with
//! rows differed, and every difference was this one.
//!
//! Measured against PostgreSQL 18.6 on the same row (`bench` oracle,
//! 2026-09-06), which is what these expectations are:
//!
//! ```text
//!   session zone UTC          all four forms  "2026-01-01T00:00:00+00:00"
//!   session zone Asia/Tokyo   all four forms  "2026-01-01T09:00:00+09:00"
//! ```
//!
//! SPG 7.40.7 answered the scalar correctly and the other three with a
//! bare `"2026-01-01T00:00:00"` — no offset, and not converted to the
//! session zone either. `Value` has `Timestamp(i64)` and no timestamptz
//! variant, so the type is the only witness and it is only in reach
//! where the argument ASTs are.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        eng.execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
    }
    eng
}

fn one(eng: &mut Engine, sql: &str) -> String {
    match eng
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"))
    {
        QueryResult::Rows { rows, .. } => match rows.first().and_then(|r| r.values.first()) {
            Some(Value::Text(s)) => s.to_string(),
            Some(Value::Json(s)) => s.to_string(),
            other => panic!("{sql:?}: unexpected {other:?}"),
        },
        other => panic!("{sql:?}: expected Rows, got {other:?}"),
    }
}

fn fixture() -> Engine {
    engine_with(&[
        "CREATE TABLE tzr (id INT, ts TIMESTAMPTZ)",
        "INSERT INTO tzr VALUES (1, TIMESTAMPTZ '2026-01-01 00:00:00+00')",
    ])
}

#[test]
fn a_row_carries_the_offset_the_scalar_already_did() {
    let mut eng = fixture();
    eng.execute("SET TimeZone = 'UTC'").expect("set");
    let scalar = one(&mut eng, "SELECT to_jsonb(ts)::text FROM tzr");
    assert_eq!(scalar, "\"2026-01-01T00:00:00+00:00\"");

    // The three that lost it. PG 18.6 answers the same instant, spelled
    // the same way, in all of them.
    assert_eq!(
        one(&mut eng, "SELECT row_to_json(t)::text FROM tzr t"),
        "{\"id\":1,\"ts\":\"2026-01-01T00:00:00+00:00\"}"
    );
    assert!(
        one(&mut eng, "SELECT to_jsonb(t)::text FROM tzr t")
            .contains("\"2026-01-01T00:00:00+00:00\""),
        "to_jsonb of a whole row"
    );
    assert!(
        one(
            &mut eng,
            "SELECT json_build_object('ts', ts)::text FROM tzr"
        )
        .contains("\"2026-01-01T00:00:00+00:00\""),
        "json_build_object"
    );
}

// The session zone is NOT pinned here, deliberately.
//
// Probed on this engine with `SET TimeZone = 'Asia/Tokyo'` in force and
// `SHOW TimeZone` answering `Asia/Tokyo`:
//
// ```text
//   to_jsonb(ts)      "2026-01-01T00:00:00+00:00"
//   row_to_json(t)    "2026-01-01T00:00:00+00:00"
//   ts::text          2026-01-01 00:00:00+00
// ```
//
// The session zone does not reach evaluation in the embedded engine at
// all — `ts::text` says so too, and that predates this change. Over the
// wire it does, measured against the published 7.40.7 image, which is
// why the zone half of this defect is pinned in the server suite
// instead. An assertion here would pass for the wrong reason or fail
// for one this fix does not own.

/// A plain `timestamp` has no zone and must not grow one.
#[test]
fn a_timestamp_without_a_zone_stays_without_one() {
    let mut eng = engine_with(&[
        "CREATE TABLE plain (id INT, ts TIMESTAMP)",
        "INSERT INTO plain VALUES (1, TIMESTAMP '2026-01-01 00:00:00')",
    ]);
    eng.execute("SET TimeZone = 'Asia/Tokyo'").expect("set");
    assert_eq!(
        one(&mut eng, "SELECT row_to_json(t)::text FROM plain t"),
        "{\"id\":1,\"ts\":\"2026-01-01T00:00:00\"}",
        "PG leaves a zoneless timestamp alone in either zone"
    );
}

/// The keys of `json_build_object` are TEXT, not JSON. Rewriting a
/// value must not reach across into the key beside it.
#[test]
fn a_key_is_still_a_key() {
    let mut eng = fixture();
    eng.execute("SET TimeZone = 'UTC'").expect("set");
    let got = one(
        &mut eng,
        "SELECT json_build_object('a', ts, 'b', id)::text FROM tzr",
    );
    assert!(got.contains("\"a\""), "{got}");
    assert!(got.contains("\"b\""), "{got}");
    assert!(got.contains("2026-01-01T00:00:00+00:00"), "{got}");
}
