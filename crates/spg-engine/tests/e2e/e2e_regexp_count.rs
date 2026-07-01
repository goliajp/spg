//! v7.37.17 (17.6 siblings) — PG 15+ regexp_count.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn as_bigint(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected BigInt, got {other:?}"),
    }
}

#[test]
fn regexp_count_basic() {
    let mut e = Engine::new();
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT regexp_count('hello world', 'l')")),
        3
    );
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT regexp_count('aaa', 'a')")),
        3
    );
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT regexp_count('abc', 'z')")),
        0
    );
}

#[test]
fn regexp_count_with_start_position() {
    let mut e = Engine::new();
    // 'hello world' — starting at position 7 (1-based) skips both
    // 'l' in "hello" (positions 3,4) and hits the 'l' in "world"
    // at position 10.
    assert_eq!(
        as_bigint(&first(
            &mut e,
            "SELECT regexp_count('hello world', 'l', 7)"
        )),
        1
    );
}

#[test]
fn regexp_count_regex_metacharacter() {
    let mut e = Engine::new();
    // Match 3-letter words separated by space.
    assert_eq!(
        as_bigint(&first(
            &mut e,
            "SELECT regexp_count('abc def ghi jklm', '[a-z][a-z][a-z]')"
        )),
        4
    );
}

#[test]
fn regexp_count_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT regexp_count(NULL::text, 'x')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT regexp_count('abc', NULL::text)"),
        spg_storage::Value::Null
    ));
}
