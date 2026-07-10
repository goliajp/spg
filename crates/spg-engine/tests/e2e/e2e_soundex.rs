//! v7.37.17 (17.6 siblings) — fuzzystrmatch soundex(text).

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
fn soundex_classic_vectors() {
    let mut e = Engine::new();
    // Classic Wikipedia examples of the Russell-Odell algo.
    assert_eq!(text(&first(&mut e, "SELECT soundex('Robert')")), "R163");
    assert_eq!(text(&first(&mut e, "SELECT soundex('Rupert')")), "R163");
    assert_eq!(text(&first(&mut e, "SELECT soundex('Rubin')")), "R150");
    assert_eq!(text(&first(&mut e, "SELECT soundex('Ashcraft')")), "A261");
    assert_eq!(text(&first(&mut e, "SELECT soundex('Tymczak')")), "T522");
    assert_eq!(text(&first(&mut e, "SELECT soundex('Pfister')")), "P236");
    assert_eq!(text(&first(&mut e, "SELECT soundex('Honeyman')")), "H555");
}

#[test]
fn soundex_padded_to_4() {
    let mut e = Engine::new();
    assert_eq!(text(&first(&mut e, "SELECT soundex('A')")), "A000");
    assert_eq!(text(&first(&mut e, "SELECT soundex('Ab')")), "A100");
    assert_eq!(text(&first(&mut e, "SELECT soundex('')")), "");
}

#[test]
fn soundex_case_insensitive() {
    let mut e = Engine::new();
    // Same code regardless of case.
    let a = text(&first(&mut e, "SELECT soundex('smith')"));
    let b = text(&first(&mut e, "SELECT soundex('SMITH')"));
    assert_eq!(a, b);
    assert_eq!(a, "S530");
}

#[test]
fn soundex_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT soundex(NULL::text)"),
        spg_storage::Value::Null
    ));
}
