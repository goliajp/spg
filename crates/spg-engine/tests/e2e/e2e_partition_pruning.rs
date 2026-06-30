//! v7.37.16 (16.7) — planner pruning for LIST / HASH partitions
//! on equality predicates.
//!
//! For RANGE-on-TIMESTAMPTZ, pruning has shipped since v7.37.6-B
//! (`select_with_where_prunes_to_overlapping_children` in the
//! Range suite). This module covers the new LIST + HASH paths
//! and the DEFAULT-child interaction:
//!
//!   - LIST: `key = 'jp'` keeps only the child whose values
//!     contain 'jp'.
//!   - HASH: `key = N` keeps only the child whose REMAINDER
//!     matches `hash(N) mod MODULUS`.
//!   - DEFAULT: scanned iff no concrete child claims the
//!     equality literal.
//!
//! Pruning is verified at the *result* level (no SELECT must
//! return rows from a child the planner shouldn't be scanning),
//! since EXPLAIN-of-pruned-list emission lands in 16.10.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows for {sql}");
    };
    rows.into_iter().map(|r| r.values).collect()
}

fn one_i64(e: &mut Engine, sql: &str) -> i64 {
    let mut rs = rows(e, sql);
    let row = rs.pop().expect("one row");
    match row.into_iter().next().expect("one col") {
        Value::BigInt(n) => n,
        Value::Int(n) => i64::from(n),
        other => panic!("expected integer, got {other:?}"),
    }
}

fn list_setup() -> Engine {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE cust (
             id BIGINT,
             region TEXT
         ) PARTITION BY LIST (region)",
    )
    .unwrap();
    e.execute("CREATE TABLE cust_apac PARTITION OF cust FOR VALUES IN ('jp', 'kr', 'tw')")
        .unwrap();
    e.execute("CREATE TABLE cust_emea PARTITION OF cust FOR VALUES IN ('de', 'fr', 'uk')")
        .unwrap();
    e.execute("CREATE TABLE cust_default PARTITION OF cust DEFAULT").unwrap();
    for (id, region) in [
        (1, "jp"),
        (2, "kr"),
        (3, "de"),
        (4, "fr"),
        (5, "us"), // DEFAULT
        (6, "br"), // DEFAULT
    ] {
        let sql = format!("INSERT INTO cust VALUES ({id}, '{region}')");
        e.execute(&sql).unwrap();
    }
    e
}

#[test]
fn list_eq_predicate_returns_only_matching_partition_rows() {
    let mut e = list_setup();
    // region = 'jp' should land entirely inside cust_apac.
    let rs = rows(&mut e, "SELECT id FROM cust WHERE region = 'jp'");
    assert_eq!(rs.len(), 1);
    assert!(matches!(rs[0][0], Value::BigInt(1)));

    // region = 'fr' should land entirely inside cust_emea.
    let rs = rows(&mut e, "SELECT id FROM cust WHERE region = 'fr'");
    assert_eq!(rs.len(), 1);
    assert!(matches!(rs[0][0], Value::BigInt(4)));

    // region = 'us' should fall into the DEFAULT child.
    let rs = rows(&mut e, "SELECT id FROM cust WHERE region = 'us'");
    assert_eq!(rs.len(), 1);
    assert!(matches!(rs[0][0], Value::BigInt(5)));
}

#[test]
fn list_no_predicate_returns_every_partition_row() {
    let mut e = list_setup();
    assert_eq!(one_i64(&mut e, "SELECT COUNT(*) FROM cust"), 6);
}

#[test]
fn list_eq_on_unmatched_value_still_returns_default_rows() {
    // No equality predicate => DEFAULT must always be scanned.
    // With one (region = 'us'), DEFAULT is the only viable
    // child since no concrete LIST contains 'us'. SELECT must
    // still find the row.
    let mut e = list_setup();
    let count = one_i64(&mut e, "SELECT COUNT(*) FROM cust WHERE region = 'us'");
    assert_eq!(count, 1);
}

fn hash_setup() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE oh (id BIGINT, label TEXT) PARTITION BY HASH (id)")
        .unwrap();
    for r in 0..4 {
        let sql = format!(
            "CREATE TABLE oh_{r} PARTITION OF oh FOR VALUES WITH (MODULUS 4, REMAINDER {r})"
        );
        e.execute(&sql).unwrap();
    }
    for i in 0..100 {
        let sql = format!("INSERT INTO oh VALUES ({i}, 'r{i}')");
        e.execute(&sql).unwrap();
    }
    e
}

#[test]
fn hash_eq_predicate_returns_unique_row() {
    let mut e = hash_setup();
    // Every key 0..100 inserted exactly once. id = 42 must
    // return exactly one row regardless of pruning (correctness
    // gate).
    let rs = rows(&mut e, "SELECT id, label FROM oh WHERE id = 42");
    assert_eq!(rs.len(), 1);
    assert!(matches!(rs[0][0], Value::BigInt(42)));
}

#[test]
fn hash_eq_predicate_consistent_with_total_count() {
    let mut e = hash_setup();
    // Loop over 0..100 with eq predicate; sum must equal total
    // count (== 100). This is the strongest correctness gate:
    // a buggy pruner that drops the wrong child would lose
    // some rows.
    let mut sum = 0;
    for i in 0..100 {
        let sql = format!("SELECT COUNT(*) FROM oh WHERE id = {i}");
        sum += one_i64(&mut e, &sql);
    }
    assert_eq!(sum, 100);
}

#[test]
fn spg_partition_health_lists_parent_and_every_child() {
    let mut e = list_setup();
    // 1 parent + 2 LIST children + 1 DEFAULT = 4 rows.
    let rs = rows(&mut e, "SELECT * FROM spg_partition_health");
    // cust (Parent), cust_apac (List), cust_default (Default), cust_emea (List)
    assert_eq!(rs.len(), 4);
    // Columns: parent_name (0), partition_name (1), role (2), row_count (3), bound_desc (4)
    let names_roles: Vec<(String, String)> = rs
        .iter()
        .map(|r| {
            let Value::Text(n) = &r[1] else { panic!("name") };
            let Value::Text(role) = &r[2] else { panic!("role") };
            (n.to_string(), role.to_string())
        })
        .collect();
    assert!(
        names_roles
            .iter()
            .any(|(n, role)| n == "cust" && role == "Parent")
    );
    assert!(
        names_roles
            .iter()
            .any(|(n, role)| n == "cust_apac" && role == "List")
    );
    assert!(
        names_roles
            .iter()
            .any(|(n, role)| n == "cust_emea" && role == "List")
    );
    assert!(
        names_roles
            .iter()
            .any(|(n, role)| n == "cust_default" && role == "Default")
    );
    // Row counts sum across leaves == 6 (the 6 inserts in
    // list_setup). Parent's row_count column reads as 0 because
    // parent itself never stores rows.
    let total_leaf_rows: i64 = rs
        .iter()
        .filter(|r| {
            let Value::Text(role) = &r[2] else { return false };
            role.as_ref() != "Parent"
        })
        .map(|r| match r[3] {
            Value::BigInt(n) => n,
            _ => 0,
        })
        .sum();
    assert_eq!(total_leaf_rows, 6);
}

#[test]
fn range_eq_predicate_still_prunes_correctly() {
    // Regression guard: 16.7 added DEFAULT-only-when-no-eq
    // logic in the same code path Range relies on. Make sure
    // the existing Range pruning still works.
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE events (
             id BIGINT,
             received_at TIMESTAMPTZ
         ) PARTITION BY RANGE (received_at)",
    )
    .unwrap();
    e.execute(
        "CREATE TABLE events_2026_06 PARTITION OF events \
         FOR VALUES FROM ('2026-06-01 00:00:00+00') TO ('2026-07-01 00:00:00+00')",
    )
    .unwrap();
    e.execute(
        "CREATE TABLE events_2026_07 PARTITION OF events \
         FOR VALUES FROM ('2026-07-01 00:00:00+00') TO ('2026-08-01 00:00:00+00')",
    )
    .unwrap();
    e.execute("CREATE TABLE events_default PARTITION OF events DEFAULT")
        .unwrap();
    e.execute("INSERT INTO events VALUES (1, '2026-06-15 12:00:00+00')")
        .unwrap();
    e.execute("INSERT INTO events VALUES (2, '2026-07-15 12:00:00+00')")
        .unwrap();
    let count = one_i64(
        &mut e,
        "SELECT COUNT(*) FROM events WHERE received_at >= '2026-07-01 00:00:00+00'",
    );
    assert_eq!(count, 1);
}
