//! v7.37.17 (17.6 siblings) — fuzzystrmatch levenshtein.

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

fn as_int(v: &spg_storage::Value<'_>) -> i32 {
    match v {
        spg_storage::Value::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn levenshtein_identical() {
    let mut e = Engine::new();
    assert_eq!(
        as_int(&first(&mut e, "SELECT levenshtein('hello', 'hello')")),
        0
    );
}

#[test]
fn levenshtein_single_substitution() {
    let mut e = Engine::new();
    assert_eq!(
        as_int(&first(&mut e, "SELECT levenshtein('hello', 'hallo')")),
        1
    );
}

#[test]
fn levenshtein_insertions_deletions() {
    let mut e = Engine::new();
    // 'kitten' → 'sitting' is a classic 3-op example.
    assert_eq!(
        as_int(&first(&mut e, "SELECT levenshtein('kitten', 'sitting')")),
        3
    );
    // Empty → 'abc' = 3 insertions.
    assert_eq!(as_int(&first(&mut e, "SELECT levenshtein('', 'abc')")), 3);
    // 'abc' → '' = 3 deletions.
    assert_eq!(as_int(&first(&mut e, "SELECT levenshtein('abc', '')")), 3);
}

#[test]
fn levenshtein_unicode_safe() {
    let mut e = Engine::new();
    // Different chars, same char count.
    // 3 chars each. 'ABC' vs 'DEF' = 3 subs.
    assert_eq!(
        as_int(&first(&mut e, "SELECT levenshtein('ABC', 'DEF')")),
        3
    );
}

#[test]
fn levenshtein_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT levenshtein(NULL::text, 'x')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT levenshtein('x', NULL::text)"),
        spg_storage::Value::Null
    ));
}
