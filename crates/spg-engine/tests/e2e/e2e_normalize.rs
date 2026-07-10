//! v7.37.17 (17.6 siblings) — PG 13+ normalize + is_normalized.

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

fn as_bool(v: &spg_storage::Value<'_>) -> bool {
    match v {
        spg_storage::Value::Bool(b) => *b,
        other => panic!("expected Bool, got {other:?}"),
    }
}

#[test]
fn normalize_nfc_composes() {
    let mut e = Engine::new();
    // 'e' + COMBINING ACUTE (U+0301) composes to é (U+00E9) under NFC.
    let v = text(&first(&mut e, "SELECT normalize(unistr('e\\0301'))"));
    assert_eq!(v, "é");
    assert_eq!(v.chars().count(), 1);
}

#[test]
fn normalize_nfd_decomposes() {
    let mut e = Engine::new();
    // é (U+00E9) decomposes to 'e' + U+0301 under NFD.
    let v = text(&first(&mut e, "SELECT normalize('é', 'NFD')"));
    assert_eq!(v.chars().count(), 2);
    let mut it = v.chars();
    assert_eq!(it.next(), Some('e'));
    assert_eq!(it.next(), Some('\u{0301}'));
}

#[test]
fn normalize_nfkc_folds_compat() {
    let mut e = Engine::new();
    // U+FB01 LATIN SMALL LIGATURE FI folds to 'fi' under NFKC.
    let v = text(&first(&mut e, "SELECT normalize(unistr('\\FB01'), 'NFKC')"));
    assert_eq!(v, "fi");
}

#[test]
fn is_normalized_checks() {
    let mut e = Engine::new();
    // Precomposed é IS NFC-normalized.
    assert!(as_bool(&first(&mut e, "SELECT is_normalized('é')")));
    // Decomposed e+combining IS NOT NFC-normalized.
    assert!(!as_bool(&first(
        &mut e,
        "SELECT is_normalized(unistr('e\\0301'))"
    )));
    // But it IS NFD-normalized.
    assert!(as_bool(&first(
        &mut e,
        "SELECT is_normalized(unistr('e\\0301'), 'NFD')"
    )));
}

#[test]
fn normalize_unknown_form_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT normalize('x', 'NFX')").is_err());
}

#[test]
fn normalize_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT normalize(NULL::text)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT is_normalized(NULL::text)"),
        spg_storage::Value::Null
    ));
}
