//! v7.37.17 (17.6 siblings) — catalog function forms of the
//! jsonb ? / ?| / ?& / @> / <@ operators.

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

fn boolean(v: &spg_storage::Value<'_>) -> bool {
    match v {
        spg_storage::Value::Bool(b) => *b,
        other => panic!("expected Bool, got {other:?}"),
    }
}

#[test]
fn exists_single_key() {
    let mut e = Engine::new();
    assert!(boolean(&first(
        &mut e,
        r#"SELECT jsonb_exists('{"a":1,"b":2}', 'a')"#
    )));
    assert!(!boolean(&first(
        &mut e,
        r#"SELECT jsonb_exists('{"a":1}', 'z')"#
    )));
}

#[test]
fn exists_any_and_all() {
    let mut e = Engine::new();
    assert!(boolean(&first(
        &mut e,
        r#"SELECT jsonb_exists_any('{"a":1,"b":2}', ARRAY['z', 'b'])"#
    )));
    assert!(!boolean(&first(
        &mut e,
        r#"SELECT jsonb_exists_any('{"a":1}', ARRAY['y', 'z'])"#
    )));
    assert!(boolean(&first(
        &mut e,
        r#"SELECT jsonb_exists_all('{"a":1,"b":2}', ARRAY['a', 'b'])"#
    )));
    assert!(!boolean(&first(
        &mut e,
        r#"SELECT jsonb_exists_all('{"a":1}', ARRAY['a', 'z'])"#
    )));
}

#[test]
fn contains_and_contained() {
    let mut e = Engine::new();
    // @> — left contains right.
    assert!(boolean(&first(
        &mut e,
        r#"SELECT jsonb_contains('{"a":1,"b":2}', '{"a":1}')"#
    )));
    assert!(!boolean(&first(
        &mut e,
        r#"SELECT jsonb_contains('{"a":1}', '{"a":2}')"#
    )));
    // <@ — left contained in right (swapped delegation).
    assert!(boolean(&first(
        &mut e,
        r#"SELECT jsonb_contained('{"a":1}', '{"a":1,"b":2}')"#
    )));
    assert!(!boolean(&first(
        &mut e,
        r#"SELECT jsonb_contained('{"z":9}', '{"a":1}')"#
    )));
}

#[test]
fn exists_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        r#"jsonb_exists(NULL::text, 'a')"#,
        r#"jsonb_contains(NULL::text, '{}')"#,
        r#"jsonb_contained('{}', NULL::text)"#,
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
