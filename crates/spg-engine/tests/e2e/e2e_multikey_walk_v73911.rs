//! v7.39.11 — a multi-column `ORDER BY` walks the index that holds
//! exactly that ordering.
//!
//! Reported by sentori against 7.39.10: `ORDER BY a, b LIMIT 10` on a
//! table indexed for it planned as `Seq Scan -> Sort` here, against an
//! `Incremental Sort` over an index scan on PostgreSQL 18. The gate
//! refused anything but a single ORDER BY term.
//!
//! Keys sort by the whole tuple, so `iter_asc` over a composite B-tree
//! IS `ORDER BY a, b`; the walk needed permission, not machinery.
//! Three things have to hold, and every one of them is the tree's
//! limitation rather than a conservative choice — the negative controls
//! below are one per reason:
//!
//!   * the terms are the index's key columns, in its order, from the
//!     leading one;
//!   * every term runs the same direction, because the tree is walked
//!     one way for all of them;
//!   * every key column is NOT NULL, because a NULL key is not in the
//!     tree and the pass that emits those rows places them for one
//!     column, not for a tuple.
//!
//! Every expectation is the answer the same statement gives when the
//! walk declines — an ordering is only served correctly if the walked
//! form and the sorted form agree.

use spg_engine::{Engine, QueryResult};

fn q(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    rows.iter()
        .map(|r| {
            r.values
                .iter()
                .map(spg_engine::eval::value_to_text)
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect()
}

fn seeded(with_index: bool) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d (a int NOT NULL, b int NOT NULL, c int)")
        .unwrap();
    e.execute("INSERT INTO d VALUES (2,20,1),(1,30,2),(2,10,3),(1,10,4)")
        .unwrap();
    if with_index {
        e.execute("CREATE INDEX d_ab ON d (a, b)").unwrap();
    }
    e
}

fn agrees(sql: &str) -> Vec<String> {
    let indexed = q(&mut seeded(true), sql);
    let plain = q(&mut seeded(false), sql);
    assert_eq!(indexed, plain, "{sql}: the index changed the answer");
    indexed
}

#[test]
fn two_keys_in_the_indexs_own_order_are_walked() {
    let mut e = seeded(true);
    let p = q(&mut e, "EXPLAIN SELECT a, b FROM d ORDER BY a, b LIMIT 3");
    assert!(
        p.iter().any(|l| l.contains("Index Scan using d_ab")),
        "the walk was not named: {p:?}"
    );
    assert!(
        p.iter().any(|l| l.contains("Order By: a, b")),
        "a two-key walk named one key: {p:?}"
    );
    assert!(
        !p.iter().any(|l| l.trim_start().starts_with("Sort")),
        "a walked order still claimed a Sort: {p:?}"
    );
}

#[test]
fn the_rows_are_the_ones_the_sort_would_have_given() {
    assert_eq!(
        agrees("SELECT a, b FROM d ORDER BY a, b"),
        ["1,10", "1,30", "2,10", "2,20"]
    );
    assert_eq!(
        agrees("SELECT a, b FROM d ORDER BY a, b LIMIT 3"),
        ["1,10", "1,30", "2,10"]
    );
    assert_eq!(
        agrees("SELECT a, b FROM d ORDER BY a DESC, b DESC"),
        ["2,20", "2,10", "1,30", "1,10"]
    );
}

/// Mixed directions: the tree is walked one way for every key, so
/// `a ASC, b DESC` is not an ordering it holds. PostgreSQL serves this
/// from an index whose SECOND key is declared descending; SPG's index
/// does not scan per column.
#[test]
fn mixed_directions_still_sort() {
    let mut e = seeded(true);
    let p = q(&mut e, "EXPLAIN SELECT a, b FROM d ORDER BY a, b DESC");
    assert!(
        p.iter().any(|l| l.trim_start().starts_with("Sort")),
        "an ordering the tree does not hold was claimed as a walk: {p:?}"
    );
    assert_eq!(
        agrees("SELECT a, b FROM d ORDER BY a, b DESC"),
        ["1,30", "1,10", "2,20", "2,10"]
    );
}

/// A permutation of the key columns is a different ordering.
#[test]
fn the_wrong_key_order_still_sorts() {
    let mut e = seeded(true);
    let p = q(&mut e, "EXPLAIN SELECT a, b FROM d ORDER BY b, a");
    assert!(
        p.iter().any(|l| l.trim_start().starts_with("Sort")),
        "{p:?}"
    );
    assert_eq!(
        agrees("SELECT a, b FROM d ORDER BY b, a"),
        ["1,10", "2,10", "2,20", "1,30"]
    );
}

/// A term the index does not carry.
#[test]
fn a_key_outside_the_index_still_sorts() {
    let mut e = seeded(true);
    let p = q(&mut e, "EXPLAIN SELECT a, c FROM d ORDER BY a, c");
    assert!(
        p.iter().any(|l| l.trim_start().starts_with("Sort")),
        "{p:?}"
    );
    assert_eq!(
        agrees("SELECT a, c FROM d ORDER BY a, c"),
        ["1,2", "1,4", "2,1", "2,3"]
    );
}

/// A nullable key column. Its NULL rows are not in the tree, and the
/// pass that emits them places them for one column, not for a tuple.
#[test]
fn a_nullable_key_column_still_sorts() {
    let build = |with_index: bool| {
        let mut e = Engine::new();
        e.execute("CREATE TABLE n (a int NOT NULL, b int)").unwrap();
        e.execute("INSERT INTO n VALUES (1,10),(1,NULL),(2,5)")
            .unwrap();
        if with_index {
            e.execute("CREATE INDEX n_ab ON n (a, b)").unwrap();
        }
        e
    };
    let mut e = build(true);
    let p = q(&mut e, "EXPLAIN SELECT a, b FROM n ORDER BY a, b");
    assert!(
        p.iter().any(|l| l.trim_start().starts_with("Sort")),
        "a tuple with a NULL key was claimed as a walk: {p:?}"
    );
    let sql = "SELECT a, b FROM n ORDER BY a, b";
    assert_eq!(q(&mut build(true), sql), q(&mut build(false), sql));
}
