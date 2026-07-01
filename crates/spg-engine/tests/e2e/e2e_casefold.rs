//! v7.37.17 (17.6 siblings) — PG 18 casefold(text).

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
fn casefold_ascii() {
    let mut e = Engine::new();
    assert_eq!(text(&first(&mut e, "SELECT casefold('HELLO World')")), "hello world");
}

#[test]
fn casefold_unicode() {
    let mut e = Engine::new();
    // German sharp s: 'ß'.to_lowercase() stays ß in Rust's simple
    // mapping, but 'ẞ' (capital sharp s U+1E9E) folds to ß.
    assert_eq!(text(&first(&mut e, "SELECT casefold('STRAẞE')")), "straße");
    // Turkish dotted capital İ folds to i + combining dot in full
    // Unicode lowercase; verify no crash + non-empty.
    let v = text(&first(&mut e, "SELECT casefold('İSTANBUL')"));
    assert!(v.starts_with('i'));
    // Greek sigma: Rust's to_lowercase applies the word-final
    // sigma rule — trailing Σ becomes ς (final form), interior
    // Σ becomes σ. This matches full Unicode casing.
    assert_eq!(text(&first(&mut e, "SELECT casefold('ΣΟΦΟΣ')")), "σοφος");
}

#[test]
fn casefold_matches_caseless_equality() {
    let mut e = Engine::new();
    // Typical usage: caseless comparison.
    let a = text(&first(&mut e, "SELECT casefold('HeLLo')"));
    let b = text(&first(&mut e, "SELECT casefold('hEllO')"));
    assert_eq!(a, b);
}

#[test]
fn casefold_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT casefold(NULL::text)"),
        spg_storage::Value::Null
    ));
}
