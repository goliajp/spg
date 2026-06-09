//! v7.17.0 Phase 3.P0-33 — MySQL `YEAR` type.
//!
//! Reference:
//!   https://dev.mysql.com/doc/refman/8.0/en/year.html
//!
//! Surface:
//!   * `CREATE TABLE … (col YEAR)` — DDL accept.
//!   * `INSERT … VALUES (1999)` / `VALUES ('1999')` — int / text
//!     literal both accepted.
//!   * `SELECT col FROM t` — Value::Year(u16) round-trip.
//!   * Catalog snapshot survival (encode + decode tag 26).
//!   * NULL handling.
//!
//! Invariants pinned:
//!   * Storage: u16 in range 1901..=2155 (MySQL canonical), with
//!     0 as the "zero year" sentinel — same range MySQL allows.
//!   * Display: always 4 digits, zero-padded. `0` renders as
//!     `0000` (MySQL canonical zero-year text form).
//!   * Out-of-range input → hard SQL error (no silent truncation).
//!   * Two-digit YEAR (`'99'` → 1999, `'15'` → 2015) NOT supported
//!     in v7.17.0 — deprecated in MySQL 5.7+ and not in scope.

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
fn ddl_accepts_year_keyword() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, born YEAR)")
        .unwrap();
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let schema = cat.get("t").unwrap().schema();
    assert!(matches!(schema.columns[1].ty, DataType::Year));
}

#[test]
fn insert_int_literal_round_trip() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, born YEAR)",
        "INSERT INTO t VALUES (1, 1999)",
    ]);
    let rows = select(&mut eng, "SELECT born FROM t");
    let Value::Year(y) = &rows[0][0] else {
        panic!("expected Year, got {:?}", rows[0][0]);
    };
    assert_eq!(*y, 1999);
}

#[test]
fn insert_text_literal_round_trip() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, born YEAR)",
        "INSERT INTO t VALUES (1, '2025')",
    ]);
    let rows = select(&mut eng, "SELECT born FROM t");
    let Value::Year(y) = &rows[0][0] else {
        panic!()
    };
    assert_eq!(*y, 2025);
}

#[test]
fn boundary_values_round_trip() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, born YEAR)",
        "INSERT INTO t VALUES (1, 1901), (2, 2155), (3, 0)",
    ]);
    let rows = select(&mut eng, "SELECT id, born FROM t ORDER BY id");
    let years: Vec<u16> = rows
        .iter()
        .map(|r| match &r[1] {
            Value::Year(y) => *y,
            _ => panic!(),
        })
        .collect();
    assert_eq!(years, vec![1901, 2155, 0]);
}

#[test]
fn year_column_survives_catalog_round_trip() {
    let mut eng = engine_with(&[
        "CREATE TABLE c (id INT NOT NULL, born YEAR)",
        "INSERT INTO c VALUES (1, 1985), (2, 2007)",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let mut eng2 = Engine::restore(cat);
    let rows = select(&mut eng2, "SELECT born FROM c ORDER BY born");
    assert_eq!(rows.len(), 2);
    let Value::Year(a) = &rows[0][0] else {
        panic!()
    };
    let Value::Year(b) = &rows[1][0] else {
        panic!()
    };
    assert_eq!((*a, *b), (1985, 2007));
}

#[test]
fn year_null_column() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, born YEAR)",
        "INSERT INTO t VALUES (1, NULL)",
    ]);
    let rows = select(&mut eng, "SELECT born FROM t");
    assert!(matches!(rows[0][0], Value::Null));
}

#[test]
fn out_of_range_year_is_error() {
    let mut eng = engine_with(&["CREATE TABLE t (id INT NOT NULL, born YEAR)"]);
    let r = eng.execute("INSERT INTO t VALUES (1, 1900)");
    assert!(r.is_err(), "year 1900 (below 1901) must error");
    let r = eng.execute("INSERT INTO t VALUES (1, 2156)");
    assert!(r.is_err(), "year 2156 (above 2155) must error");
}

#[test]
fn malformed_text_year_is_error() {
    let mut eng = engine_with(&["CREATE TABLE t (id INT NOT NULL, born YEAR)"]);
    let r = eng.execute("INSERT INTO t VALUES (1, 'not a year')");
    assert!(r.is_err(), "garbage YEAR text must error");
}
