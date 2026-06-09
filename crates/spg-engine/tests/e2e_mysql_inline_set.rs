//! v7.17.0 Phase 3.P0-37 — MySQL inline `SET('a','b','c','d')`.
//!
//! Reference:
//!   https://dev.mysql.com/doc/refman/8.0/en/set.html
//!
//! Surface:
//!   * `CREATE TABLE … (col SET('read','write','admin'))` — DDL
//!     accept with inline value list.
//!   * `INSERT … VALUES ('read,write')` — comma-separated subset
//!     of the variant list; each token validated.
//!   * `SELECT col FROM t` — returns canonical comma-joined text
//!     in definition order, duplicates dropped.
//!   * Catalog snapshot survival (FILE_VERSION 42+ — same
//!     appendix shape as inline ENUM).
//!   * NULL handling.
//!
//! Invariants pinned:
//!   * Storage: canonical comma-joined text in DEFINITION order
//!     (not insertion order). `'write,read'` and `'read,write'`
//!     both store/select as `'read,write'` when the column is
//!     `SET('read','write')`.
//!   * Duplicates in input → de-duplicated on storage.
//!   * Empty string → stored as `''` (matches MySQL: "no flags set").
//!   * Token not in the variant list → hard SQL error.
//!   * NULL preserved (no auto-coerce to `''`).

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

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
fn ddl_accepts_inline_set() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, perms SET('read','write','admin'))")
        .unwrap();
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let schema = cat.get("t").unwrap().schema();
    assert_eq!(
        schema.columns[1].inline_set_variants.as_deref(),
        Some(&["read".to_string(), "write".to_string(), "admin".to_string()][..])
    );
}

#[test]
fn insert_subset_round_trips() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, perms SET('read','write','admin'))",
        "INSERT INTO t VALUES (1, 'read,write')",
    ]);
    let rows = select(&mut eng, "SELECT perms FROM t");
    let Value::Text(s) = &rows[0][0] else { panic!() };
    assert_eq!(s, "read,write");
}

#[test]
fn insert_canonicalises_to_definition_order() {
    // Input out-of-order → output in declaration order.
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, perms SET('read','write','admin'))",
        "INSERT INTO t VALUES (1, 'admin,read')",
    ]);
    let rows = select(&mut eng, "SELECT perms FROM t");
    let Value::Text(s) = &rows[0][0] else { panic!() };
    assert_eq!(s, "read,admin");
}

#[test]
fn insert_dedups_repeated_tokens() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, perms SET('read','write','admin'))",
        "INSERT INTO t VALUES (1, 'read,read,write,write')",
    ]);
    let rows = select(&mut eng, "SELECT perms FROM t");
    let Value::Text(s) = &rows[0][0] else { panic!() };
    assert_eq!(s, "read,write");
}

#[test]
fn insert_empty_string_means_no_flags() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, perms SET('read','write','admin'))",
        "INSERT INTO t VALUES (1, '')",
    ]);
    let rows = select(&mut eng, "SELECT perms FROM t");
    let Value::Text(s) = &rows[0][0] else { panic!() };
    assert_eq!(s, "");
}

#[test]
fn insert_single_value_round_trips() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, perms SET('read','write','admin'))",
        "INSERT INTO t VALUES (1, 'admin')",
    ]);
    let rows = select(&mut eng, "SELECT perms FROM t");
    let Value::Text(s) = &rows[0][0] else { panic!() };
    assert_eq!(s, "admin");
}

#[test]
fn insert_unlisted_token_is_error() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, perms SET('read','write','admin'))",
    ]);
    let r = eng.execute("INSERT INTO t VALUES (1, 'read,superpower')");
    assert!(r.is_err(), "non-listed SET token must error");
}

#[test]
fn insert_null_preserved() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, perms SET('read','write'))",
        "INSERT INTO t VALUES (1, NULL)",
    ]);
    let rows = select(&mut eng, "SELECT perms FROM t");
    assert!(matches!(rows[0][0], Value::Null));
}

#[test]
fn set_column_survives_catalog_round_trip() {
    let mut eng = engine_with(&[
        "CREATE TABLE files (id INT NOT NULL, perms SET('r','w','x'))",
        "INSERT INTO files VALUES (1, 'r,w'), (2, 'r,w,x'), (3, '')",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let mut eng2 = Engine::restore(cat);
    let rows = select(&mut eng2, "SELECT id, perms FROM files ORDER BY id");
    assert_eq!(rows.len(), 3);
    let Value::Text(a) = &rows[0][1] else { panic!() };
    let Value::Text(b) = &rows[1][1] else { panic!() };
    let Value::Text(c) = &rows[2][1] else { panic!() };
    assert_eq!(a, "r,w");
    assert_eq!(b, "r,w,x");
    assert_eq!(c, "");
}

#[test]
fn ddl_rejects_empty_set_list() {
    let mut eng = Engine::new();
    let r = eng.execute("CREATE TABLE t (id INT NOT NULL, p SET())");
    assert!(r.is_err(), "SET with empty value list must error");
}
