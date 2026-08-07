//! v7.39 (round 551) — EXPLAIN named an index that cannot serve the
//! condition.
//!
//! The audit's phase 5 opens with "EXPLAIN 名实对齐 (minimal, do first —
//! it is the prerequisite observation for B1)". Measured, it was not an
//! alignment nicety: the scan node named the table's FIRST BTree index
//! whatever the predicate was, with a comment saying so — "Approximation:
//! report the table's first BTree index name (single-index tables — the
//! common case — are exact)". On a table with a primary key and a
//! secondary index it is a plain misstatement:
//!
//!     CREATE TABLE e (id INT PRIMARY KEY, k INT);
//!     CREATE INDEX ek ON e (k);
//!     EXPLAIN SELECT * FROM e WHERE k BETWEEN 10 AND 12;
//!     -> Index Scan using e_pkey on e          ← an index on `id`
//!
//! EXPLAIN is the first thing any perf investigation reads — this
//! project's own included — so an instrument that misnames the access
//! path is worse than one that says nothing. The index the condition
//! actually keys on is named now.
//!
//! The same node printed the predicate TWICE for a two-sided range:
//! once as `Index Cond` and again as a `Filter` beneath it, which reads
//! as a re-check that does not happen. A range seek takes the whole
//! predicate as one seek, so there is no residual.
//!
//! Recorded, not fixed: the row estimate for a range still reads 1
//! where 150 match, and `ORDER BY <pk> LIMIT n` plans a Sort over a Seq
//! Scan where PG walks the index. The second is a planner choice (the
//! audit's phase 4), not a naming lie — EXPLAIN reports what SPG really
//! does.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE e551 (id INT PRIMARY KEY, k INT, v TEXT)")
        .unwrap();
    e.execute("INSERT INTO e551 SELECT g, g % 100, 'x' FROM generate_series(1, 500) g")
        .unwrap();
    e.execute("CREATE INDEX e551k ON e551 (k)").unwrap();
    e
}

fn plan(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The index named is the one keyed on the condition's column.
#[test]
fn round551_explain_names_the_serving_index() {
    let mut e = engine();
    let range = plan(
        &mut e,
        "EXPLAIN SELECT * FROM e551 WHERE k BETWEEN 10 AND 12",
    );
    assert!(
        range[0].contains("Index Scan using e551k on e551"),
        "named the wrong index: {range:?}"
    );
    // And the primary key is still named for a key lookup.
    let pk = plan(&mut e, "EXPLAIN SELECT * FROM e551 WHERE id = 42");
    assert!(
        pk[0].contains("Index Scan using e551_pkey on e551"),
        "{pk:?}"
    );
}

/// A range seek prints its predicate once.
#[test]
fn round551_no_duplicate_filter_under_an_index_cond() {
    let mut e = engine();
    let range = plan(
        &mut e,
        "EXPLAIN SELECT * FROM e551 WHERE k BETWEEN 10 AND 12",
    );
    assert_eq!(
        range.iter().filter(|l| l.contains("Index Cond:")).count(),
        1,
        "{range:?}"
    );
    assert_eq!(
        range.iter().filter(|l| l.contains("Filter:")).count(),
        0,
        "the seek took the whole predicate, so there is no residual: {range:?}"
    );
}

/// A composite predicate still splits: the indexed conjunct to Index
/// Cond, the rest to a Filter — round 226's behaviour, unchanged.
#[test]
fn round551_a_residual_still_becomes_a_filter() {
    let mut e = engine();
    let p = plan(
        &mut e,
        "EXPLAIN SELECT * FROM e551 WHERE id = 42 AND v = 'x'",
    );
    assert!(p[0].contains("Index Scan using e551_pkey"), "{p:?}");
    assert!(
        p.iter().any(|l| l.contains("Index Cond: (id = 42)")),
        "{p:?}"
    );
    assert!(p.iter().any(|l| l.contains("Filter:")), "{p:?}");
}

/// An unindexed predicate still says Seq Scan, naming no index.
#[test]
fn round551_an_unindexed_predicate_names_nothing() {
    let mut e = engine();
    let p = plan(&mut e, "EXPLAIN SELECT * FROM e551 WHERE v = 'x'");
    assert!(p[0].contains("Seq Scan on e551"), "{p:?}");
    assert!(
        !p.iter().any(|l| l.contains("Index")),
        "a seq scan must not name an index: {p:?}"
    );
}
