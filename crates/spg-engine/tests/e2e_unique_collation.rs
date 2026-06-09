//! v7.17.0 Phase 3.P0-45 — UNIQUE constraint honours the column's
//! collation.
//!
//! Phase 2.5b (`d3c731c`) wired the `Collation` field into the
//! schema so GROUP BY / ORDER BY / `=`-comparisons fold case for
//! `*_ci` columns, but the UNIQUE-constraint enforcement (and the
//! per-index UNIQUE check on INSERT) still compared `Value::Text`
//! byte-wise. A MySQL dump with `name VARCHAR(64) COLLATE
//! utf8mb4_0900_ai_ci UNIQUE` would let `('Foo')` and `('FOO')`
//! coexist in SPG even though MySQL would reject the second.
//!
//! These tests pin the post-fix surface so a `*_ci` column folds
//! case during the UNIQUE check, the same way GROUP BY does.

use spg_engine::Engine;

#[test]
fn ci_inline_unique_rejects_case_variant() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE t (s TEXT COLLATE \"case_insensitive\" NOT NULL UNIQUE)",
    )
    .unwrap();
    e.execute("INSERT INTO t VALUES ('Foo')").unwrap();
    let err = e
        .execute("INSERT INTO t VALUES ('FOO')")
        .expect_err("case-insensitive UNIQUE must reject 'FOO' after 'Foo'");
    let msg = format!("{err:?}");
    assert!(
        msg.to_lowercase().contains("unique"),
        "unexpected error shape: {msg}"
    );
}

#[test]
fn ci_inline_unique_accepts_distinct_values() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE t (s TEXT COLLATE \"case_insensitive\" NOT NULL UNIQUE)",
    )
    .unwrap();
    e.execute("INSERT INTO t VALUES ('Foo'), ('bar'), ('baz')")
        .unwrap();
}

#[test]
fn binary_collation_keeps_case_sensitive_unique() {
    // Binary (default) collation: `'Foo'` and `'FOO'` coexist.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (s TEXT NOT NULL UNIQUE)").unwrap();
    e.execute("INSERT INTO t VALUES ('Foo'), ('FOO')").unwrap();
}

#[test]
fn ci_unique_rejects_within_same_batch() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE t (s TEXT COLLATE \"case_insensitive\" NOT NULL UNIQUE)",
    )
    .unwrap();
    let err = e
        .execute("INSERT INTO t VALUES ('Foo'), ('foo')")
        .expect_err("case-insensitive UNIQUE must reject within-batch case variant");
    let msg = format!("{err:?}");
    assert!(msg.to_lowercase().contains("unique"));
}

#[test]
fn composite_unique_constraint_folds_per_column_collation() {
    // Composite UNIQUE across (a, b): a binary, b case-insensitive.
    // Two rows with same `a` and same-case-folded `b` should collide.
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE t (\
             a TEXT NOT NULL, \
             b TEXT COLLATE \"case_insensitive\" NOT NULL, \
             UNIQUE (a, b)\
         )",
    )
    .unwrap();
    e.execute("INSERT INTO t VALUES ('x', 'Foo')").unwrap();
    let err = e
        .execute("INSERT INTO t VALUES ('x', 'FOO')")
        .expect_err("composite UNIQUE must fold the ci column");
    let msg = format!("{err:?}");
    assert!(msg.to_lowercase().contains("unique"));
    // Distinct leading column lets the same b coexist.
    e.execute("INSERT INTO t VALUES ('y', 'FOO')").unwrap();
}

#[test]
fn create_unique_index_rejects_existing_case_dup() {
    // Pre-fill with case-variant duplicates, then CREATE UNIQUE
    // INDEX on a ci column must detect the existing collision.
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE t (s TEXT COLLATE \"case_insensitive\" NOT NULL)",
    )
    .unwrap();
    e.execute("INSERT INTO t VALUES ('Foo'), ('FOO')").unwrap();
    let err = e
        .execute("CREATE UNIQUE INDEX idx_s ON t (s)")
        .expect_err("CREATE UNIQUE INDEX must see the ci duplicate");
    let msg = format!("{err:?}");
    assert!(msg.to_lowercase().contains("unique") || msg.to_lowercase().contains("violate"));
}

#[test]
fn null_keys_still_lift_out_of_check() {
    // PG / MySQL both allow multiple NULLs in a UNIQUE column by
    // default — collation must not change that.
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE t (s TEXT COLLATE \"case_insensitive\" NULL UNIQUE)",
    )
    .unwrap();
    e.execute("INSERT INTO t VALUES (NULL), (NULL), ('Foo')")
        .unwrap();
}
