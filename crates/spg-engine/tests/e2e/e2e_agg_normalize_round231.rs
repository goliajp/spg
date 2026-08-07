//! v7.39 (round 231) — following up round 230's "verified the common
//! aggregates, not the cold ones" by actually running the cold ones.
//! Three real defects fell out, none of them window-specific:
//!
//!   * `every(x)` — standard SQL's alias for `bool_and` — was admitted by
//!     `is_aggregate_name` but unknown to `classify_agg_name`, whose
//!     unknown arm is a `panic!`. `every(v) OVER (…)` therefore ABORTED
//!     the query. The GROUP BY builder had folded the alias at its own
//!     call sites; the fold now lives in one place.
//!   * `range_agg` returned its inputs verbatim instead of the normalized
//!     multirange PG produces — `[1,3),[5,9),[2,6)` came back as three
//!     spans where PG merges them into the single `{[1,9)}` they cover.
//!     A GROUP BY bug, not just a window one.
//!   * casting text to a multirange kept the literal's spans as written,
//!     so `'{[1,3),[3,5)}'::int4multirange` printed two abutting spans.
//!     The constructor function already normalized; the cast did not.
//!
//! All expectations diffed against live PG18.4 (2026-07-19).

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Null => "NULL".to_string(),
            v => spg_engine::eval::value_to_text(v),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

fn ranges() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE rr (g text, r int4range)").unwrap();
    e.execute(
        "INSERT INTO rr VALUES ('a','[1,3)'),('a','[5,9)'),('a','[2,6)'),\
         ('b','[1,2)'),('b','[3,4)'),('b','[10,20)'),\
         ('c','[1,3)'),('c','[3,5)'),('f','[1,3)'),('f',NULL)",
    )
    .unwrap();
    e
}

#[test]
fn every_is_an_alias_for_bool_and_everywhere() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE w (g text, v int)").unwrap();
    e.execute("INSERT INTO w VALUES ('a',10),('a',20),('b',1),('b',30)")
        .unwrap();
    // Used to abort the query with a classifier panic.
    assert_eq!(
        text(
            &mut e,
            "SELECT every(v > 5) OVER (PARTITION BY g)::text FROM w"
        ),
        "true"
    );
    assert_eq!(
        text(&mut e, "SELECT every(v > 5)::text FROM w WHERE g = 'b'"),
        "false"
    );
    assert_eq!(
        text(&mut e, "SELECT EVERY(v > 5)::text FROM w WHERE g = 'a'"),
        "true"
    );
}

#[test]
fn range_agg_returns_a_normalized_multirange() {
    let mut e = ranges();
    // Overlapping spans collapse to their union.
    assert_eq!(
        text(&mut e, "SELECT range_agg(r)::text FROM rr WHERE g = 'a'"),
        "{[1,9)}"
    );
    // Disjoint spans stay separate, sorted by lower bound.
    assert_eq!(
        text(&mut e, "SELECT range_agg(r)::text FROM rr WHERE g = 'b'"),
        "{[1,2),[3,4),[10,20)}"
    );
    // Abutting spans merge too ([1,3) and [3,5) share the boundary).
    assert_eq!(
        text(&mut e, "SELECT range_agg(r)::text FROM rr WHERE g = 'c'"),
        "{[1,5)}"
    );
    // NULL inputs are skipped, not collected.
    assert_eq!(
        text(&mut e, "SELECT range_agg(r)::text FROM rr WHERE g = 'f'"),
        "{[1,3)}"
    );
    // The window form goes through the same finalize.
    assert_eq!(
        text(
            &mut e,
            "SELECT range_agg(r) OVER (PARTITION BY g)::text FROM rr WHERE g = 'a'"
        ),
        "{[1,9)}"
    );
}

#[test]
fn text_to_multirange_cast_normalizes_like_the_constructor() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT '{[1,3),[3,5),[9,10)}'::int4multirange::text"
        ),
        "{[1,5),[9,10)}"
    );
    // Same answer the constructor already gave.
    assert_eq!(
        text(
            &mut e,
            "SELECT int4multirange(int4range(1,3), int4range(3,5), int4range(9,10))::text"
        ),
        "{[1,5),[9,10)}"
    );
    // Out-of-order input sorts.
    assert_eq!(
        text(&mut e, "SELECT '{[9,10),[1,3)}'::int4multirange::text"),
        "{[1,3),[9,10)}"
    );
}

#[test]
fn ordered_set_aggregates_name_themselves_when_given_over() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE w (g text, v int)").unwrap();
    // PG distinguishes an ordered-set aggregate from a plain
    // `agg(x ORDER BY y)`; round 230 gave both the generic message.
    let got = format!(
        "{}",
        e.execute(
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY v) OVER (PARTITION BY g) FROM w"
        )
        .unwrap_err()
    );
    assert!(
        got.contains("OVER is not supported for ordered-set aggregate percentile_cont"),
        "{got}"
    );
    let got = format!(
        "{}",
        e.execute("SELECT array_agg(v ORDER BY v) OVER (PARTITION BY g) FROM w")
            .unwrap_err()
    );
    assert!(
        got.contains("aggregate ORDER BY is not implemented for window functions"),
        "{got}"
    );
}
