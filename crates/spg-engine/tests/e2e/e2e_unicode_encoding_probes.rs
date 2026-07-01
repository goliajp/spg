//! v7.37.17 (17.6 siblings) — unicode_version + pg_encoding_max_length.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

fn as_int(v: &spg_storage::Value<'_>) -> i32 {
    match v {
        spg_storage::Value::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn unicode_version_returns_15() {
    let mut e = Engine::new();
    assert_eq!(text(&first(&mut e, "SELECT unicode_version()")), "15.0");
    assert_eq!(text(&first(&mut e, "SELECT icu_unicode_version()")), "15.0");
}

#[test]
fn pg_encoding_max_length_utf8() {
    let mut e = Engine::new();
    // UTF-8 = 4 bytes max per codepoint.
    assert_eq!(as_int(&first(&mut e, "SELECT pg_encoding_max_length(6)")), 4);
}

#[test]
fn pg_encoding_max_length_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_encoding_max_length(NULL::int)"),
        spg_storage::Value::Null
    ));
}
