//! v7.38 (read01 P6.11) — the regex `x` (extended) flag: unescaped whitespace
//! outside a character class is ignored and `#` starts an end-of-line comment,
//! so a pattern can be spaced out for readability. Oracle values from PG 18.4.

use spg_engine::{Engine, QueryResult};

fn scalar(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!("expected rows"),
    }
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        spg_storage::Value::TextArray(items) => items[0].as_deref().unwrap_or("").to_string(),
        other => panic!("expected text/array, got {other:?}"),
    }
}

#[test]
fn extended_flag_ignores_unescaped_whitespace() {
    let mut e = Engine::new();
    assert_eq!(text(&scalar(&mut e, "SELECT substring('abc' from '(?x) a b c')")), "abc");
}

#[test]
fn extended_flag_strips_hash_comments() {
    let mut e = Engine::new();
    assert_eq!(
        text(&scalar(&mut e, "SELECT regexp_match('foo123bar', '(?x) \\d+  # the digits')")),
        "123"
    );
}

#[test]
fn extended_flag_keeps_escaped_and_class_whitespace() {
    let mut e = Engine::new();
    // Escaped space is a literal space.
    assert_eq!(text(&scalar(&mut e, "SELECT regexp_match('a b','(?x)a\\ b')")), "a b");
    // Space inside a character class is literal.
    assert_eq!(text(&scalar(&mut e, "SELECT regexp_match('a b','(?x)[ ]')")), " ");
}

#[test]
fn without_extended_flag_whitespace_is_literal() {
    let mut e = Engine::new();
    assert_eq!(text(&scalar(&mut e, "SELECT substring('a b c' from 'a b c')")), "a b c");
}
