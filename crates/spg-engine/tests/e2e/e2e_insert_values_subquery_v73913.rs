//! v7.39.13 — a scalar subquery in an `INSERT … VALUES` list runs.
//!
//! Reported by sentori against 7.39.12, and not a regression: it is a
//! path the `ORDER BY` fix of that version did not reach. Every type
//! raised the same internal message, and PG 18.6 accepts all of them:
//!
//! ```text
//!   INSERT INTO d1 (n)  VALUES ((SELECT max(n)  FROM s))   7
//!   INSERT INTO d2 (t)  VALUES ((SELECT max(t)  FROM s))   x
//!   INSERT INTO d3 (ts) VALUES ((SELECT max(ts) FROM s))   1767225600
//!
//!   SPG 7.39.12: ERROR: subquery reached row eval — engine resolver bug
//! ```
//!
//! The message names itself an engine bug because it is one: the row
//! walk is not allowed to meet a subquery. UPDATE has resolved its
//! assignments up front since embed round-12 and DELETE its WHERE since
//! round 157; INSERT never did, so the subquery was still in the tree
//! when evaluation reached it.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    spg_engine::eval::value_to_text(&rows[0].values[0])
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE s (n int, t text)",
        "INSERT INTO s VALUES (7, 'x')",
        "CREATE TABLE d1 (n int)",
        "CREATE TABLE d2 (t text)",
    ] {
        e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"));
    }
    e
}

#[test]
fn a_scalar_subquery_in_a_values_list_is_evaluated() {
    let mut e = seeded();
    e.execute("INSERT INTO d1 (n) VALUES ((SELECT max(n) FROM s))")
        .expect("PG 18.6 accepts this");
    assert_eq!(one(&mut e, "SELECT n FROM d1"), "7");
}

#[test]
fn the_same_holds_for_a_text_column() {
    let mut e = seeded();
    e.execute("INSERT INTO d2 (t) VALUES ((SELECT max(t) FROM s))")
        .expect("PG 18.6 accepts this");
    assert_eq!(one(&mut e, "SELECT t FROM d2"), "x");
}

/// And beside a literal, so the resolution is per-expression rather
/// than a whole-tuple special case.
#[test]
fn a_subquery_beside_a_literal_in_the_same_tuple() {
    let mut e = seeded();
    e.execute("CREATE TABLE d4 (n int, t text)").unwrap();
    e.execute("INSERT INTO d4 (n, t) VALUES ((SELECT max(n) FROM s), 'lit')")
        .expect("PG 18.6 accepts this");
    assert_eq!(one(&mut e, "SELECT n FROM d4"), "7");
    assert_eq!(one(&mut e, "SELECT t FROM d4"), "lit");
}

/// Multi-row VALUES: each tuple's subquery resolves.
#[test]
fn every_tuple_of_a_multi_row_values_resolves() {
    let mut e = seeded();
    e.execute("INSERT INTO d1 (n) VALUES ((SELECT max(n) FROM s)), ((SELECT min(n) FROM s))")
        .expect("PG 18.6 accepts this");
    assert_eq!(one(&mut e, "SELECT count(*) FROM d1"), "2");
}
