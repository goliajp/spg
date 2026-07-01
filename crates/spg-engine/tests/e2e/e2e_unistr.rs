//! v7.37.17 (17.6 siblings) — PG 16+ unistr(text).

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

#[test]
fn unistr_no_escapes() {
    let mut e = Engine::new();
    assert_eq!(text(&first(&mut e, "SELECT unistr('hello')")), "hello");
    assert_eq!(text(&first(&mut e, "SELECT unistr('')")), "");
}

#[test]
fn unistr_hex_4digit() {
    let mut e = Engine::new();
    // U+00E9 = é
    assert_eq!(
        text(&first(&mut e, r"SELECT unistr('caf\00e9')")),
        "café"
    );
}

#[test]
fn unistr_hex_u_syntax() {
    let mut e = Engine::new();
    // é → é
    assert_eq!(
        text(&first(&mut e, r"SELECT unistr('café')")),
        "café"
    );
}

#[test]
fn unistr_hex_plus_6digit() {
    let mut e = Engine::new();
    // \+01F600 → 😀 (U+1F600 emoji)
    assert_eq!(
        text(&first(&mut e, r"SELECT unistr('emoji \+01F600 here')")),
        "emoji 😀 here"
    );
}

#[test]
fn unistr_hex_uppercase_u_syntax() {
    let mut e = Engine::new();
    // \U0001F600 → 😀
    assert_eq!(
        text(&first(&mut e, r"SELECT unistr('emoji \U0001F600')")),
        "emoji 😀"
    );
}

#[test]
fn unistr_backslash_backslash() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, r"SELECT unistr('a\\b')")),
        r"a\b"
    );
}

#[test]
fn unistr_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT unistr(NULL::text)"),
        spg_storage::Value::Null
    ));
}
