//! v7.37.17 (17.6 siblings) — PG 18 casefold(text).

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

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn casefold_ascii() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT casefold('HELLO World')")),
        "hello world"
    );
}

#[test]
fn casefold_unicode() {
    let mut e = Engine::new();
    // German sharp s: 'ß'.to_lowercase() stays ß in Rust's simple
    // mapping, but 'ẞ' (capital sharp s U+1E9E) folds to ß.
    assert_eq!(text(&first(&mut e, "SELECT casefold('STRAẞE')")), "straße");
    // v7.39 (round 521) — both of these described Rust's `to_lowercase`
    // rather than PG's `casefold`, and asserted accordingly. Case FOLDING is
    // position-blind on purpose: its job is to make two spellings of a word
    // compare equal, so no final-sigma rule applies. Measured on PG18.
    assert_eq!(
        text(&first(&mut e, "SELECT casefold('İSTANBUL')")),
        "istanbul"
    );
    assert_eq!(text(&first(&mut e, "SELECT casefold('ΣΟΦΟΣ')")), "σοφοσ");
    // An already-lowered final sigma is left as it is.
    assert_eq!(text(&first(&mut e, "SELECT casefold('σοφος')")), "σοφος");
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
