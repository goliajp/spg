//! v7.9.20 + v7.9.21 — runtime DEFAULT evaluation. mailrs G3 + G4.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn fake_clock() -> i64 {
    // 2026-06-04 12:00:00 UTC in microseconds since epoch.
    1_780_536_000_000_000
}

fn engine_with_clock() -> Engine {
    Engine::new().with_clock(fake_clock)
}

#[test]
fn default_now_is_filled_at_insert_time() {
    let mut eng = engine_with_clock();
    eng.execute("CREATE TABLE t (id INT NOT NULL, created_at TIMESTAMP NOT NULL DEFAULT now())")
        .unwrap();
    eng.execute("INSERT INTO t (id) VALUES (1)").unwrap();
    let r = eng.execute("SELECT created_at FROM t").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows[0].values[0], Value::Timestamp(fake_clock()));
}

#[test]
fn default_current_timestamp_keyword_no_parens() {
    let mut eng = engine_with_clock();
    eng.execute(
        "CREATE TABLE t (id INT NOT NULL, ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP)",
    )
    .unwrap();
    eng.execute("INSERT INTO t (id) VALUES (1)").unwrap();
    let r = eng.execute("SELECT ts FROM t").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert!(matches!(rows[0].values[0], Value::Timestamp(_)));
}

#[test]
fn default_current_date_yields_a_date() {
    let mut eng = engine_with_clock();
    eng.execute("CREATE TABLE t (id INT NOT NULL, report_date DATE NOT NULL DEFAULT CURRENT_DATE)")
        .unwrap();
    eng.execute("INSERT INTO t (id) VALUES (1)").unwrap();
    let r = eng.execute("SELECT report_date FROM t").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert!(matches!(rows[0].values[0], Value::Date(_)));
}

#[test]
fn literal_default_still_works() {
    let mut eng = engine_with_clock();
    eng.execute("CREATE TABLE t (id INT NOT NULL, name TEXT DEFAULT 'unset' NOT NULL)")
        .unwrap();
    eng.execute("INSERT INTO t (id) VALUES (1)").unwrap();
    let r = eng.execute("SELECT name FROM t").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows[0].values[0], Value::Text("unset".into()));
}

#[test]
fn multiple_runtime_defaults_in_same_row() {
    // mailrs accounts pattern: created_at + updated_at both DEFAULT now().
    let mut eng = engine_with_clock();
    eng.execute(
        "CREATE TABLE accounts (\
            id INT NOT NULL,\
            created_at TIMESTAMP NOT NULL DEFAULT now(),\
            updated_at TIMESTAMP NOT NULL DEFAULT now()\
        )",
    )
    .unwrap();
    eng.execute("INSERT INTO accounts (id) VALUES (42)")
        .unwrap();
    let r = eng
        .execute("SELECT created_at, updated_at FROM accounts")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows[0].values[0], Value::Timestamp(fake_clock()));
    assert_eq!(rows[0].values[1], Value::Timestamp(fake_clock()));
}

#[test]
fn runtime_default_survives_catalog_snapshot() {
    let mut eng = engine_with_clock();
    eng.execute("CREATE TABLE t (id INT NOT NULL, created_at TIMESTAMP NOT NULL DEFAULT now())")
        .unwrap();
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let col = &cat.get("t").unwrap().schema().columns[1];
    assert!(col.runtime_default.is_some());
}

#[test]
fn explicit_value_overrides_runtime_default() {
    let mut eng = engine_with_clock();
    eng.execute("CREATE TABLE t (id INT NOT NULL, created_at TIMESTAMP NOT NULL DEFAULT now())")
        .unwrap();
    // Explicit value wins over the default.
    eng.execute("INSERT INTO t (id, created_at) VALUES (1, '2020-01-01 00:00:00')")
        .unwrap();
    let r = eng.execute("SELECT created_at FROM t").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    // Not the clock's value.
    assert_ne!(rows[0].values[0], Value::Timestamp(fake_clock()));
}
