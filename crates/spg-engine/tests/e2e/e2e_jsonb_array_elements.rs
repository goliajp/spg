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
fn select_list_expands_per_element() {
    // v7.38 (read01, T15) — jsonb/json_array_elements[_text] in the SELECT list
    // (not FROM) expand to one row per element, matching PG (they collapsed to a
    // single TextArray row before). Covers no-FROM, no-FROM with a sibling
    // scalar column, and over a real table's rows. Oracle: live PG 18.4.
    let mut e = Engine::new();
    // no-FROM, plain and _text.
    assert_eq!(
        texts(&rows(&mut e, "SELECT jsonb_array_elements('[1, 2, 3]'::jsonb)")),
        [Some("1".into()), Some("2".into()), Some("3".into())]
    );
    assert_eq!(
        texts(&rows(&mut e, "SELECT jsonb_array_elements_text('[\"a\", null]'::jsonb)")),
        [Some("a".into()), None]
    );
    // no-FROM with a sibling scalar column repeated per element.
    let got = rows(&mut e, "SELECT 'x', jsonb_array_elements('[7, 8]'::jsonb)");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0][0], spg_storage::Value::Text("x".into()));
    assert_eq!(got[1][1], spg_storage::Value::Text("8".into()));
    // Over a real table: one element-row per source row, in order.
    e.execute("CREATE TABLE jt(j jsonb)").unwrap();
    e.execute("INSERT INTO jt VALUES ('[10, 20]'), ('[30]')").unwrap();
    assert_eq!(
        texts(&rows(&mut e, "SELECT jsonb_array_elements(j) FROM jt")),
        [Some("10".into()), Some("20".into()), Some("30".into())]
    );
    // NULL input → no rows.
    assert_eq!(rows(&mut e, "SELECT jsonb_array_elements(NULL::jsonb)").len(), 0);
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
