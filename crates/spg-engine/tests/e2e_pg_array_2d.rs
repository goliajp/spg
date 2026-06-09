//! v7.17.0 Phase 3.P0-40 — 2-dimensional arrays for INT / TEXT
//! / BIGINT. Builds on Phase 7.10 (TEXT[]) / 7.11 (INT[]/BIGINT[])
//! by adding a second dimension. PG's `int[][]` and `text[][]`
//! are widely used for matrix-shaped columns (image tiles, sensor
//! grids, RBAC permission matrices).
//!
//! Surface:
//!   * `CREATE TABLE … (col INT[][])` — DDL accept.
//!   * Text literal `'{{1,2},{3,4}}'` parses into a 2-row 2-col
//!     matrix.
//!   * Display: nested `'{{1,2},{3,4}}'` external form.
//!   * Catalog snapshot survival.
//!   * NULL preserved (entire cell or individual elements).
//!
//! Invariants pinned:
//!   * Storage: `Vec<Vec<Option<T>>>` row-major.
//!   * All rows must have the same column count (matches PG
//!     enforcement); ragged literals → hard SQL error.
//!   * Empty `'{}'` → empty 2D array (0 rows).
//!   * NULL element token `NULL` (case-insensitive) → None.

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
fn ddl_accepts_2d_int_array() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, m INT[][])")
        .unwrap();
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let schema = cat.get("t").unwrap().schema();
    assert!(matches!(schema.columns[1].ty, DataType::IntArray2D));
}

#[test]
fn ddl_accepts_2d_text_array() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, m TEXT[][])")
        .unwrap();
    let cat = spg_storage::Catalog::deserialize(&eng.snapshot()).unwrap();
    let schema = cat.get("t").unwrap().schema();
    assert!(matches!(schema.columns[1].ty, DataType::TextArray2D));
}

#[test]
fn ddl_accepts_2d_bigint_array() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, m BIGINT[][])")
        .unwrap();
    let cat = spg_storage::Catalog::deserialize(&eng.snapshot()).unwrap();
    let schema = cat.get("t").unwrap().schema();
    assert!(matches!(schema.columns[1].ty, DataType::BigIntArray2D));
}

#[test]
fn insert_int_2d_round_trips() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, m INT[][])",
        "INSERT INTO t VALUES (1, '{{1,2,3},{4,5,6}}')",
    ]);
    let rows = select(&mut eng, "SELECT m FROM t");
    let Value::IntArray2D(rows2d) = &rows[0][0] else {
        panic!("expected IntArray2D, got {:?}", rows[0][0]);
    };
    assert_eq!(rows2d.len(), 2);
    assert_eq!(rows2d[0].len(), 3);
    assert_eq!(rows2d[0], vec![Some(1), Some(2), Some(3)]);
    assert_eq!(rows2d[1], vec![Some(4), Some(5), Some(6)]);
}

#[test]
fn insert_text_2d_round_trips() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, m TEXT[][])",
        "INSERT INTO t VALUES (1, '{{a,b},{c,d}}')",
    ]);
    let rows = select(&mut eng, "SELECT m FROM t");
    let Value::TextArray2D(rows2d) = &rows[0][0] else { panic!() };
    assert_eq!(rows2d.len(), 2);
    assert_eq!(rows2d[0][0].as_deref(), Some("a"));
    assert_eq!(rows2d[1][1].as_deref(), Some("d"));
}

#[test]
fn insert_bigint_2d_round_trips() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, m BIGINT[][])",
        "INSERT INTO t VALUES (1, '{{9999999999,8888888888}}')",
    ]);
    let rows = select(&mut eng, "SELECT m FROM t");
    let Value::BigIntArray2D(rows2d) = &rows[0][0] else { panic!() };
    assert_eq!(rows2d.len(), 1);
    assert_eq!(rows2d[0][0], Some(9_999_999_999));
}

#[test]
fn insert_null_elements_preserved() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, m INT[][])",
        "INSERT INTO t VALUES (1, '{{1,NULL},{NULL,4}}')",
    ]);
    let rows = select(&mut eng, "SELECT m FROM t");
    let Value::IntArray2D(rows2d) = &rows[0][0] else { panic!() };
    assert_eq!(rows2d[0][1], None);
    assert_eq!(rows2d[1][0], None);
    assert_eq!(rows2d[1][1], Some(4));
}

#[test]
fn empty_2d_array() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, m INT[][])",
        "INSERT INTO t VALUES (1, '{}')",
    ]);
    let rows = select(&mut eng, "SELECT m FROM t");
    let Value::IntArray2D(rows2d) = &rows[0][0] else { panic!() };
    assert!(rows2d.is_empty());
}

#[test]
fn null_column() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, m INT[][])",
        "INSERT INTO t VALUES (1, NULL)",
    ]);
    let rows = select(&mut eng, "SELECT m FROM t");
    assert!(matches!(rows[0][0], Value::Null));
}

#[test]
fn array_2d_column_survives_catalog_round_trip() {
    let mut eng = engine_with(&[
        "CREATE TABLE grid (id INT NOT NULL, m INT[][])",
        "INSERT INTO grid VALUES (1, '{{1,2},{3,4}}'), (2, '{{10,20,30}}')",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let mut eng2 = Engine::restore(cat);
    let rows = select(&mut eng2, "SELECT id, m FROM grid ORDER BY id");
    assert_eq!(rows.len(), 2);
    let Value::IntArray2D(g1) = &rows[0][1] else { panic!() };
    let Value::IntArray2D(g2) = &rows[1][1] else { panic!() };
    assert_eq!(g1.len(), 2);
    assert_eq!(g2.len(), 1);
    assert_eq!(g2[0].len(), 3);
}

#[test]
fn display_canonical_via_text_cast() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, m INT[][])",
        "INSERT INTO t VALUES (1, '{{1,2},{3,4}}')",
    ]);
    let r = eng.execute("SELECT m::text FROM t").unwrap();
    match r {
        QueryResult::Rows { rows, .. } => {
            let Value::Text(s) = &rows[0].values[0] else { panic!() };
            assert_eq!(s, "{{1,2},{3,4}}");
        }
        _ => panic!(),
    }
}

#[test]
fn ragged_2d_literal_is_error() {
    let mut eng = engine_with(&["CREATE TABLE t (id INT NOT NULL, m INT[][])"]);
    let r = eng.execute("INSERT INTO t VALUES (1, '{{1,2},{3,4,5}}')");
    assert!(r.is_err(), "ragged 2D literal must error");
}
