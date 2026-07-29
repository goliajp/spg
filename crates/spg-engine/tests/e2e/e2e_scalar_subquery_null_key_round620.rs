//! v7.39 (round 620) — a correlated scalar subquery answered NULL where the
//! correlation key is NULL, instead of running over the empty set.
//!
//! This is a silent wrong answer, and the differential corpus is what found
//! it. `(SELECT count(*) FROM b WHERE b.g = a.g)` on a row whose `a.g` is
//! NULL came back NULL; PG answers 0. `b.g = NULL` matches nothing, so the
//! subquery runs over an EMPTY set — which is the same situation as a
//! non-NULL key that matches no inner row — and the aggregate's own empty-set
//! value decides it: `count` answers 0, everything else answers NULL.
//!
//! The decorrelation already carried that value (`empty_default`) and already
//! applied it to non-NULL misses. A `matches!(key_v, Value::Null)` branch sat
//! in front of the lookup and short-circuited to NULL before it could. There
//! were TWO copies of that branch — the splice path and the memo path — and
//! only fixing one leaves the bug reachable, so both are here.
//!
//! It survived this long because it is invisible for most aggregates: `sum`,
//! `min`, `max`, `avg`, `bool_and`, `string_agg` all have NULL as their
//! empty-set value, so the short-circuit was accidentally right for them. The
//! `count` family is the only one it corrupts.
//!
//! A NULL probe key cannot collide with a real group either: the map builder
//! skips NULL keys when it groups the inner rows, so the inner table's own
//! NULL-keyed rows — three of them in `zb` below — are not reachable through
//! a NULL key. That is the boundary the pins carry, because matching them
//! would answer 3 where SQL says 0.
//!
//! All 18 shapes were checked against live PG18 and matched byte for byte.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE za (id INT, g INT, s TEXT)").unwrap();
    e.execute("CREATE TABLE zb (id INT, g INT, s TEXT)").unwrap();
    // za rows 3 and 5 have a NULL correlation key; row 4 has a non-NULL key
    // that matches nothing, which is the case that was always right.
    e.execute("INSERT INTO za VALUES (1,10,'a'),(2,20,'b'),(3,NULL,NULL),(4,99,'z'),(5,NULL,'q')")
        .unwrap();
    // zb has TWO rows whose own g is NULL. If a NULL key were allowed to probe
    // the map it would find them and answer 2.
    e.execute("INSERT INTO zb VALUES (1,10,'a'),(2,10,'a'),(3,20,'x'),(4,NULL,NULL),(5,NULL,'w')")
        .unwrap();
    e
}

/// The count family, which is the only one the short-circuit corrupted.
#[test]
fn round620_count_over_a_null_key_is_zero() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT a.id, (SELECT count(*) FROM zb b WHERE b.g = a.g) FROM za a ORDER BY a.id"),
        vec!["1|2", "2|1", "3|0", "4|0", "5|0"],
        "rows 3 and 5 have a NULL key: the empty set, so 0 — NOT the two \
         NULL-keyed rows sitting in zb, and NOT NULL"
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, (SELECT count(b.id) FROM zb b WHERE b.g = a.g) FROM za a ORDER BY a.id"),
        vec!["1|2", "2|1", "3|0", "4|0", "5|0"],
        "count(col) has the same empty-set value as count(*)"
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, (SELECT count(b.s) FROM zb b WHERE b.g = a.g) FROM za a ORDER BY a.id"),
        vec!["1|2", "2|1", "3|0", "4|0", "5|0"]
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, (SELECT count(DISTINCT b.g) FROM zb b WHERE b.g = a.g) FROM za a ORDER BY a.id"),
        vec!["1|1", "2|1", "3|0", "4|0", "5|0"]
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, (SELECT count(*) FROM zb b WHERE b.g = a.g AND b.id > 0) FROM za a ORDER BY a.id"),
        vec!["1|2", "2|1", "3|0", "4|0", "5|0"],
        "a correlated key beside an uncorrelated filter"
    );
}

/// The aggregates the short-circuit was accidentally right for. They must not
/// move: their empty-set value IS NULL.
#[test]
fn round620_other_aggregates_still_answer_null() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT a.id, (SELECT sum(b.id) FROM zb b WHERE b.g = a.g) FROM za a ORDER BY a.id"),
        vec!["1|3", "2|3", "3|NULL", "4|NULL", "5|NULL"]
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, (SELECT min(b.id) FROM zb b WHERE b.g = a.g) FROM za a ORDER BY a.id"),
        vec!["1|1", "2|3", "3|NULL", "4|NULL", "5|NULL"]
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, (SELECT max(b.id) FROM zb b WHERE b.g = a.g) FROM za a ORDER BY a.id"),
        vec!["1|2", "2|3", "3|NULL", "4|NULL", "5|NULL"]
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, (SELECT string_agg(b.s, ',') FROM zb b WHERE b.g = a.g) FROM za a ORDER BY a.id"),
        vec!["1|a,a", "2|x", "3|NULL", "4|NULL", "5|NULL"]
    );
}

/// The key's own shape: a text key, a computed key, and a key that misses for
/// every row.
#[test]
fn round620_key_shapes() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT a.id, (SELECT count(*) FROM zb b WHERE b.s = a.s) FROM za a ORDER BY a.id"),
        vec!["1|2", "2|0", "3|0", "4|0", "5|0"],
        "a text key — row 3's is NULL, and zb has a NULL-s row too"
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, (SELECT count(*) FROM zb b WHERE b.g = a.g + 0) FROM za a ORDER BY a.id"),
        vec!["1|2", "2|1", "3|0", "4|0", "5|0"],
        "a computed key, which stays NULL through the arithmetic"
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, (SELECT count(*) FROM zb b WHERE b.id = a.g) FROM za a ORDER BY a.id"),
        vec!["1|0", "2|0", "3|0", "4|0", "5|0"],
        "a key that matches nothing for any row — NULL and non-NULL alike"
    );
}

/// What the value feeds, since a NULL and a 0 travel very differently.
#[test]
fn round620_what_the_value_feeds() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT a.id, coalesce((SELECT count(*) FROM zb b WHERE b.g = a.g), -1) FROM za a ORDER BY a.id"),
        vec!["1|2", "2|1", "3|0", "4|0", "5|0"],
        "a COALESCE around it never fires now — the -1 was the workaround the \
         bug forced on callers"
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id FROM za a WHERE (SELECT count(*) FROM zb b WHERE b.g = a.g) = 0 ORDER BY a.id"),
        vec!["3", "4", "5"],
        "in a predicate: NULL = 0 is unknown, so rows 3 and 5 used to be \
         dropped from this result entirely"
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, (SELECT count(*) FROM zb b WHERE b.g = a.g) + 100 FROM za a ORDER BY a.id"),
        vec!["1|102", "2|101", "3|100", "4|100", "5|100"],
        "in arithmetic, where a NULL used to poison the whole expression"
    );
    assert_eq!(
        vals(&mut e, "SELECT sum((SELECT count(*) FROM zb b WHERE b.g = a.g)) FROM za a"),
        vec!["3"],
        "and under an outer aggregate"
    );
}
