//! v7.37.17 (17.6 siblings) — PG 15+ regexp_instr / regexp_substr /
//! regexp_like completing the regexp_count family.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn regexp_instr_first_match() {
    let mut e = Engine::new();
    // 'hello world' — first 'l' is at 1-based position 3.
    match first(&mut e, "SELECT regexp_instr('hello world', 'l')") {
        spg_storage::Value::Int(3) => {}
        other => panic!("got {other:?}"),
    }
    // No match → 0.
    match first(&mut e, "SELECT regexp_instr('abc', 'z')") {
        spg_storage::Value::Int(0) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn regexp_instr_nth_match() {
    let mut e = Engine::new();
    // 'hello world' 'l' positions (1-based): 3, 4, 10.
    match first(&mut e, "SELECT regexp_instr('hello world', 'l', 1, 3)") {
        spg_storage::Value::Int(10) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn regexp_instr_endoption_returns_end_pos() {
    let mut e = Engine::new();
    // 3-char match starting at 1-based 1 → end at 4 (1-based, one
    // past the last char consumed).
    match first(
        &mut e,
        "SELECT regexp_instr('abc def', '[a-z][a-z][a-z]', 1, 1, 1)",
    ) {
        spg_storage::Value::Int(4) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn regexp_substr_first_match() {
    let mut e = Engine::new();
    match first(
        &mut e,
        "SELECT regexp_substr('abc def ghi', '[a-z][a-z][a-z]')",
    ) {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "abc"),
        other => panic!("got {other:?}"),
    }
    // No match → NULL.
    assert!(matches!(
        first(&mut e, "SELECT regexp_substr('abc', 'z')"),
        spg_storage::Value::Null
    ));
}

#[test]
fn regexp_substr_nth_match() {
    let mut e = Engine::new();
    match first(
        &mut e,
        "SELECT regexp_substr('abc def ghi', '[a-z][a-z][a-z]', 1, 2)",
    ) {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "def"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn regexp_like_returns_bool() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT regexp_like('abc def', '[a-z]+')") {
        spg_storage::Value::Bool(true) => {}
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT regexp_like('123', '[a-z]+')") {
        spg_storage::Value::Bool(false) => {}
        other => panic!("got {other:?}"),
    }
}
