//! v7.37.17 (17.6 siblings) — jsonb_array_elements[_text] +
//! json_ variants, both the FROM-clause SRF form (via the unnest
//! rewrite) and the scalar TextArray surface.

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

fn texts(got: &[Vec<spg_storage::Value<'static>>]) -> Vec<Option<String>> {
    got.iter()
        .map(|r| match &r[0] {
            spg_storage::Value::Text(s) => Some(s.to_string()),
            spg_storage::Value::Null => None,
            other => panic!("expected Text/Null, got {other:?}"),
        })
        .collect()
}

#[test]
fn from_jsonb_array_elements_rows() {
    let mut e = Engine::new();
    // PG doc vector: jsonb_array_elements('[1,true, [2,false]]')
    // → 1 / true / [2, false] (as jsonb, canonical form).
    let got = rows(
        &mut e,
        "SELECT value FROM jsonb_array_elements('[1, true, [2, false]]')",
    );
    assert_eq!(
        texts(&got),
        [
            Some("1".to_string()),
            Some("true".to_string()),
            Some("[2, false]".to_string())
        ]
    );
}

#[test]
fn from_jsonb_array_elements_text_rows() {
    let mut e = Engine::new();
    // PG doc vector: jsonb_array_elements_text('["foo", "bar"]')
    // → foo / bar (unquoted).
    let got = rows(
        &mut e,
        "SELECT value FROM jsonb_array_elements_text('[\"foo\", \"bar\"]')",
    );
    assert_eq!(
        texts(&got),
        [Some("foo".to_string()), Some("bar".to_string())]
    );
    // JSON null → SQL NULL in the _text form.
    let got = rows(
        &mut e,
        "SELECT value FROM json_array_elements_text('[\"a\", null]')",
    );
    assert_eq!(texts(&got), [Some("a".to_string()), None]);
}

#[test]
fn column_alias_list_overrides_value() {
    let mut e = Engine::new();
    let got = rows(
        &mut e,
        "SELECT el FROM json_array_elements('[5, 6]') AS t(el)",
    );
    assert_eq!(
        texts(&got),
        [Some("5".to_string()), Some("6".to_string())]
    );
}

#[test]
fn count_and_where_compose() {
    let mut e = Engine::new();
    let got = rows(
        &mut e,
        "SELECT COUNT(*) FROM jsonb_array_elements_text('[\"a\",\"b\",\"c\"]')",
    );
    assert!(matches!(
        got[0][0],
        spg_storage::Value::Int(3) | spg_storage::Value::BigInt(3)
    ));
    let got = rows(
        &mut e,
        "SELECT value FROM jsonb_array_elements_text('[\"a\",\"b\"]') WHERE value = 'b'",
    );
    assert_eq!(texts(&got), [Some("b".to_string())]);
}

#[test]
fn non_array_input_errors() {
    let mut e = Engine::new();
    let err = e
        .execute("SELECT value FROM jsonb_array_elements('{\"a\": 1}')")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("non-array"),
        "unexpected error: {msg}"
    );
}
