//! v7.11.15 — BYTEA scalar operators: `||` concat, substring,
//! position. Part of Epic 3 of v7.11. BYTEA literals come from
//! BYTEA-typed columns (the parser doesn't accept `::BYTEA`
//! cast; that surface is deferred).

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn ok(eng: &mut Engine, sql: &str) -> QueryResult {
    eng.execute(sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"))
}

fn select_value(eng: &mut Engine, sql: &str) -> Value<'static> {
    match ok(eng, sql) {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .next()
            .map(|mut r| r.values.remove(0))
            .expect("at least one row"),
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn setup(eng: &mut Engine, left: &str, right: &str) {
    ok(eng, "CREATE TABLE t (a BYTEA NOT NULL, b BYTEA NOT NULL)");
    ok(eng, &format!("INSERT INTO t VALUES ('{left}', '{right}')"));
}

#[test]
fn bytea_concat_basic() {
    let mut eng = Engine::new();
    setup(&mut eng, "\\xdead", "\\xbeef");
    let v = select_value(&mut eng, "SELECT a || b FROM t");
    assert!(matches!(v, Value::Bytes(ref b) if b == &vec![0xDE, 0xAD, 0xBE, 0xEF]));
}

#[test]
fn bytea_concat_null_propagates() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a BYTEA NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES ('\\xff')");
    let v = select_value(&mut eng, "SELECT a || NULL FROM t");
    assert!(matches!(v, Value::Null));
}

#[test]
fn bytea_substring_two_args() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a BYTEA NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES ('\\x0102030405')");
    let v = select_value(&mut eng, "SELECT substring(a, 2) FROM t");
    assert!(matches!(v, Value::Bytes(ref b) if b == &vec![0x02, 0x03, 0x04, 0x05]));
}

#[test]
fn bytea_substring_three_args() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a BYTEA NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES ('\\x0102030405')");
    let v = select_value(&mut eng, "SELECT substring(a, 2, 2) FROM t");
    assert!(matches!(v, Value::Bytes(ref b) if b == &vec![0x02, 0x03]));
}

#[test]
fn bytea_substring_start_past_end() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a BYTEA NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES ('\\x0102')");
    let v = select_value(&mut eng, "SELECT substring(a, 99, 1) FROM t");
    assert!(matches!(v, Value::Bytes(ref b) if b.is_empty()));
}

#[test]
fn bytea_position_found() {
    let mut eng = Engine::new();
    setup(&mut eng, "\\x0203", "\\x01020304");
    let v = select_value(&mut eng, "SELECT position(a, b) FROM t");
    assert!(matches!(v, Value::Int(2)));
}

#[test]
fn bytea_position_absent_returns_zero() {
    let mut eng = Engine::new();
    setup(&mut eng, "\\xff", "\\x01020304");
    let v = select_value(&mut eng, "SELECT position(a, b) FROM t");
    assert!(matches!(v, Value::Int(0)));
}

#[test]
fn text_substring_works_too() {
    let mut eng = Engine::new();
    let v = select_value(&mut eng, "SELECT substring('hello world', 7, 5)");
    assert!(matches!(v, Value::Text(ref s) if s == "world"));
}

#[test]
fn text_position_works_too() {
    let mut eng = Engine::new();
    let v = select_value(&mut eng, "SELECT position('lo', 'hello world')");
    assert!(matches!(v, Value::Int(4)));
}

#[test]
fn position_empty_needle_returns_one() {
    let mut eng = Engine::new();
    let v = select_value(&mut eng, "SELECT position('', 'abc')");
    assert!(matches!(v, Value::Int(1)));
}
