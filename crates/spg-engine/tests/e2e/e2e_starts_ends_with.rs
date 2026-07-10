//! v7.37.17 (17.6 siblings) — starts_with + ends_with.

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

fn as_bool(v: &spg_storage::Value<'_>) -> bool {
    match v {
        spg_storage::Value::Bool(b) => *b,
        other => panic!("expected Bool, got {other:?}"),
    }
}

#[test]
fn starts_with_basic() {
    let mut e = Engine::new();
    assert!(as_bool(&first(
        &mut e,
        "SELECT starts_with('hello world', 'hello')"
    )));
    assert!(!as_bool(&first(
        &mut e,
        "SELECT starts_with('hello world', 'world')"
    )));
    // Empty prefix always matches.
    assert!(as_bool(&first(&mut e, "SELECT starts_with('abc', '')")));
    // Empty string never has non-empty prefix.
    assert!(!as_bool(&first(&mut e, "SELECT starts_with('', 'x')")));
    // Exact match.
    assert!(as_bool(&first(&mut e, "SELECT starts_with('abc', 'abc')")));
    // Prefix longer than string.
    assert!(!as_bool(&first(&mut e, "SELECT starts_with('ab', 'abc')")));
}

#[test]
fn ends_with_basic() {
    let mut e = Engine::new();
    assert!(as_bool(&first(
        &mut e,
        "SELECT ends_with('hello world', 'world')"
    )));
    assert!(!as_bool(&first(
        &mut e,
        "SELECT ends_with('hello world', 'hello')"
    )));
    assert!(as_bool(&first(&mut e, "SELECT ends_with('abc', '')")));
}

#[test]
fn text_starts_with_alias() {
    let mut e = Engine::new();
    assert!(as_bool(&first(
        &mut e,
        "SELECT text_starts_with('abc', 'a')"
    )));
}

#[test]
fn null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "starts_with(NULL::text, 'x')",
        "starts_with('x', NULL::text)",
        "ends_with(NULL::text, 'x')",
        "ends_with('x', NULL::text)",
        "text_starts_with(NULL::text, 'x')",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
