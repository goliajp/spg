//! v7.17.0 Phase 3.P0-36 — MySQL inline `ENUM('a', 'b', 'c')`.
//!
//! Reference:
//!   https://dev.mysql.com/doc/refman/8.0/en/enum.html
//!
//! Surface:
//!   * `CREATE TABLE … (col ENUM('red','green','blue'))` — DDL
//!     accept with inline value list.
//!   * `INSERT … VALUES ('red')` — text validates against the
//!     column's variant list; only listed values accepted.
//!   * `SELECT col FROM t` — returns the variant text.
//!   * Catalog snapshot survival (FILE_VERSION 41+ appendix
//!     carries the per-column variants).
//!   * NULL handling.
//!
//! Invariants pinned:
//!   * Storage: variant text (Value::Text) — matches the existing
//!     user_enum_type pattern (Phase 1.4 catalog enums).
//!   * INSERT of a non-listed value → hard SQL error (no silent
//!     truncation, no silent ''→empty-string fallback).
//!   * NULL → stored as NULL (column is nullable unless NOT NULL).
//!   * Catalog round-trip preserves variant order (significant in
//!     MySQL — ORDER BY enum_col sorts by definition order, not
//!     alphabetical).
//!
//! Why this matters:
//!   * MySQL schemas pin status / role / size / priority columns
//!     to ENUM constantly. Pre-P0-36 SPG dropped the column type
//!     at parse, so any Django/Rails/Laravel MySQL schema with
//!     ENUM failed CREATE TABLE.

use spg_engine::{Engine, QueryResult};
use spg_storage::{DataType, Value};

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        let r = eng
            .execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
        assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql:?}");
    }
    eng
}

fn select(eng: &mut Engine, sql: &str) -> Vec<Vec<Value>> {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn ddl_accepts_inline_enum() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, color ENUM('red','green','blue'))")
        .unwrap();
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let schema = cat.get("t").unwrap().schema();
    assert!(matches!(schema.columns[1].ty, DataType::Text));
    assert_eq!(
        schema.columns[1].inline_enum_variants.as_deref(),
        Some(&["red".to_string(), "green".to_string(), "blue".to_string()][..])
    );
}

#[test]
fn insert_listed_value_round_trips() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, color ENUM('red','green','blue'))",
        "INSERT INTO t VALUES (1, 'green')",
    ]);
    let rows = select(&mut eng, "SELECT color FROM t");
    let Value::Text(s) = &rows[0][0] else {
        panic!("expected Text, got {:?}", rows[0][0]);
    };
    assert_eq!(s, "green");
}

#[test]
fn insert_unlisted_value_is_error() {
    let mut eng =
        engine_with(&["CREATE TABLE t (id INT NOT NULL, color ENUM('red','green','blue'))"]);
    let r = eng.execute("INSERT INTO t VALUES (1, 'purple')");
    assert!(
        r.is_err(),
        "non-listed enum value must error, not silently store"
    );
}

#[test]
fn insert_null_preserved() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, color ENUM('red','green'))",
        "INSERT INTO t VALUES (1, NULL)",
    ]);
    let rows = select(&mut eng, "SELECT color FROM t");
    assert!(matches!(rows[0][0], Value::Null));
}

#[test]
fn inline_enum_column_survives_catalog_round_trip() {
    let mut eng = engine_with(&[
        "CREATE TABLE users (id INT NOT NULL, role ENUM('admin','editor','viewer'))",
        "INSERT INTO users VALUES (1, 'admin'), (2, 'editor'), (3, 'viewer')",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let mut eng2 = Engine::restore(cat);
    let rows = select(&mut eng2, "SELECT id, role FROM users ORDER BY id");
    assert_eq!(rows.len(), 3);
    let Value::Text(r1) = &rows[0][1] else {
        panic!()
    };
    let Value::Text(r2) = &rows[1][1] else {
        panic!()
    };
    let Value::Text(r3) = &rows[2][1] else {
        panic!()
    };
    assert_eq!(r1, "admin");
    assert_eq!(r2, "editor");
    assert_eq!(r3, "viewer");
}

#[test]
fn ddl_rejects_empty_enum_list() {
    let mut eng = Engine::new();
    let r = eng.execute("CREATE TABLE t (id INT NOT NULL, color ENUM())");
    assert!(
        r.is_err(),
        "ENUM with empty value list must be rejected at parse"
    );
}

#[test]
fn variants_preserved_after_round_trip() {
    // Variant ORDER matters in MySQL (used by `ORDER BY col`).
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, sz ENUM('small','medium','large','xlarge'))",
        "INSERT INTO t VALUES (1, 'large')",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let schema = cat.get("t").unwrap().schema();
    assert_eq!(
        schema.columns[1].inline_enum_variants.as_deref(),
        Some(
            &[
                "small".to_string(),
                "medium".to_string(),
                "large".to_string(),
                "xlarge".to_string()
            ][..]
        )
    );
}
