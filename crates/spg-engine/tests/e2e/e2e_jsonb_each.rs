//! v7.37.17 (17.6 siblings) — jsonb_each / json_each /
//! json_each_text complete the each SRF family (jsonb_each_text
//! shipped in v7.37.43-T4.5). The plain forms keep JSON rendering
//! in the value column: strings keep quotes, JSON null stays
//! jsonb 'null' (not SQL NULL).

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter()
        .map(|row| row.values.into_iter().collect())
        .collect()
}

fn render(v: &spg_storage::Value<'_>) -> Option<String> {
    match v {
        spg_storage::Value::Text(s) => Some(s.to_string()),
        spg_storage::Value::Json(s) => Some(s.to_string()),
        spg_storage::Value::Null => None,
        other => panic!("expected Text/Json/Null, got {other:?}"),
    }
}

#[test]
fn jsonb_each_keeps_json_rendering() {
    let mut e = Engine::new();
    // PG doc vector: jsonb_each('{"a":"foo", "b":"bar"}')
    // → (a, "foo") / (b, "bar") — value keeps its quotes.
    let got = rows(
        &mut e,
        "SELECT key, value FROM jsonb_each('{\"a\": \"foo\", \"b\": \"bar\"}') ORDER BY key",
    );
    assert_eq!(got.len(), 2);
    assert_eq!(render(&got[0][0]), Some("a".to_string()));
    assert_eq!(render(&got[0][1]), Some("\"foo\"".to_string()));
    assert_eq!(render(&got[1][0]), Some("b".to_string()));
    assert_eq!(render(&got[1][1]), Some("\"bar\"".to_string()));
}

#[test]
fn jsonb_each_json_null_stays_jsonb_null() {
    let mut e = Engine::new();
    let got = rows(&mut e, "SELECT value FROM jsonb_each('{\"k\": null}')");
    // The plain form renders JSON null as jsonb 'null', not SQL NULL.
    assert_eq!(render(&got[0][0]), Some("null".to_string()));
    // The _text form maps JSON null to SQL NULL.
    let got = rows(&mut e, "SELECT value FROM json_each_text('{\"k\": null}')");
    assert_eq!(render(&got[0][0]), None);
}

#[test]
fn json_each_nested_and_numbers() {
    let mut e = Engine::new();
    let got = rows(
        &mut e,
        "SELECT key, value FROM json_each('{\"n\": 5, \"o\": {\"x\": 1}}') ORDER BY key",
    );
    assert_eq!(render(&got[0][1]), Some("5".to_string()));
    assert_eq!(render(&got[1][1]), Some("{\"x\":1}".to_string()));
}

#[test]
fn each_text_still_unwraps() {
    let mut e = Engine::new();
    // Existing jsonb_each_text behaviour must be unchanged: strings
    // lose their quotes.
    let got = rows(
        &mut e,
        "SELECT value FROM jsonb_each_text('{\"a\": \"foo\"}')",
    );
    assert_eq!(render(&got[0][0]), Some("foo".to_string()));
}

#[test]
fn non_object_input_errors() {
    let mut e = Engine::new();
    let err = e
        .execute("SELECT * FROM jsonb_each('[1, 2]')")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("non-object"), "unexpected error: {msg}");
}
