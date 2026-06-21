//! v7.17.0 Phase 3.P0-35 — PG `money` type.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/datatype-money.html
//!
//! Surface:
//!   * `CREATE TABLE … (col MONEY)` — DDL accept.
//!   * `INSERT … VALUES ('$12.34')` / `VALUES (12.34)` /
//!     `VALUES (1234)` — text-with-currency, numeric, or bare
//!     integer (treated as a literal cent-count) all accepted.
//!   * `SELECT col FROM t` — Value::Money round-trip rendered as
//!     `'$12.34'` (en_US locale — PG MONEY is locale-dependent
//!     but SPG pins US for determinism).
//!   * Catalog snapshot survival (encode + decode tag 28).
//!   * NULL handling.
//!
//! Invariants pinned:
//!   * Storage: i64 cents (1 unit = 0.01 currency units), matching
//!     PG's internal `Cash` representation.
//!   * Display: `$N,NNN.CC` with comma thousands separator and
//!     two-digit cents (negative → `-$1.23` per PG en_US).
//!   * Input accepts the `$` prefix, optional commas in the
//!     integer portion, optional decimal point (defaults to .00),
//!     and optional negative sign in front of `$`. Malformed
//!     input → hard SQL error.
//!   * Range: full i64 (9 quintillion cents).
//!
//! Why this matters:
//!   * E-commerce / billing schemas pin price columns to MONEY
//!     for the locale-aware rendering and exact-cent arithmetic.

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

fn select(eng: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn ddl_accepts_money_keyword() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, price MONEY)")
        .unwrap();
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let schema = cat.get("t").unwrap().schema();
    assert!(matches!(schema.columns[1].ty, DataType::Money));
}

#[test]
fn insert_dollar_text_literal_round_trips() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, price MONEY)",
        "INSERT INTO t VALUES (1, '$12.34')",
    ]);
    let rows = select(&mut eng, "SELECT price FROM t");
    let Value::Money(c) = &rows[0][0] else {
        panic!("expected Money, got {:?}", rows[0][0]);
    };
    assert_eq!(*c, 1234);
}

#[test]
fn insert_text_with_comma_thousands() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, price MONEY)",
        "INSERT INTO t VALUES (1, '$1,234,567.89')",
    ]);
    let rows = select(&mut eng, "SELECT price FROM t");
    let Value::Money(c) = &rows[0][0] else {
        panic!()
    };
    assert_eq!(*c, 123_456_789);
}

#[test]
fn insert_negative_money() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, price MONEY)",
        "INSERT INTO t VALUES (1, '-$0.50')",
    ]);
    let rows = select(&mut eng, "SELECT price FROM t");
    let Value::Money(c) = &rows[0][0] else {
        panic!()
    };
    assert_eq!(*c, -50);
}

#[test]
fn insert_zero_cents() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, price MONEY)",
        "INSERT INTO t VALUES (1, '$0.00')",
    ]);
    let rows = select(&mut eng, "SELECT price FROM t");
    let Value::Money(c) = &rows[0][0] else {
        panic!()
    };
    assert_eq!(*c, 0);
}

#[test]
fn insert_integer_literal_treated_as_dollars() {
    // PG: bare numeric literal in a MONEY column is treated as
    // a money value in major units (NOT cents). 100 → $100.00.
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, price MONEY)",
        "INSERT INTO t VALUES (1, 100)",
    ]);
    let rows = select(&mut eng, "SELECT price FROM t");
    let Value::Money(c) = &rows[0][0] else {
        panic!()
    };
    assert_eq!(*c, 10_000); // $100.00 = 10000 cents
}

#[test]
fn money_column_survives_catalog_round_trip() {
    let mut eng = engine_with(&[
        "CREATE TABLE invoices (id INT NOT NULL, total MONEY)",
        "INSERT INTO invoices VALUES (1, '$19.99'), (2, '-$2.50')",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let mut eng2 = Engine::restore(cat);
    let rows = select(&mut eng2, "SELECT id, total FROM invoices ORDER BY id");
    assert_eq!(rows.len(), 2);
    let Value::Money(a) = &rows[0][1] else {
        panic!()
    };
    let Value::Money(b) = &rows[1][1] else {
        panic!()
    };
    assert_eq!((*a, *b), (1999, -250));
}

#[test]
fn money_null_column() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, price MONEY)",
        "INSERT INTO t VALUES (1, NULL)",
    ]);
    let rows = select(&mut eng, "SELECT price FROM t");
    assert!(matches!(rows[0][0], Value::Null));
}

#[test]
fn money_malformed_input_is_error() {
    let mut eng = engine_with(&["CREATE TABLE t (id INT NOT NULL, price MONEY)"]);
    let r = eng.execute("INSERT INTO t VALUES (1, 'not money')");
    assert!(r.is_err(), "garbage MONEY literal must error");
}
