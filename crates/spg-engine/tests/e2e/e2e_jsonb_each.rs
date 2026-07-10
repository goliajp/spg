//! v7.37.17 (17.6 siblings) — jsonb_each / json_each /
//! json_each_text complete the each SRF family (jsonb_each_text
//! shipped in v7.37.43-T4.5). The plain forms keep JSON rendering
//! in the value column: strings keep quotes, JSON null stays
//! jsonb 'null' (not SQL NULL).

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
    assert_eq!(render(&got[1][1]), Some("{\"x\": 1}".to_string()));
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
    let err = e.execute("SELECT * FROM jsonb_each('[1, 2]')").unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("non-object"), "unexpected error: {msg}");
}

#[test]
fn jsonb_to_recordset_and_record() {
    use spg_storage::Value;
    let mut e = Engine::new();

    // jsonb_to_recordset: one typed row per array element; a missing key
    // is NULL. Verified vs live PG18.4.
    let got = rows(
        &mut e,
        "SELECT a, b FROM jsonb_to_recordset('[{\"a\":1,\"b\":\"hi\"},{\"a\":2}]') AS x(a int, b text) ORDER BY a",
    );
    assert_eq!(got.len(), 2);
    assert_eq!(got[0][0], Value::Int(1));
    assert!(matches!(&got[0][1], Value::Text(s) if s == "hi"));
    assert_eq!(got[1][0], Value::Int(2));
    assert_eq!(got[1][1], Value::Null); // missing "b"

    // Scalar jsonb_to_record: a single row projected off the object.
    let got = rows(
        &mut e,
        "SELECT a, b FROM jsonb_to_record('{\"a\":7,\"b\":\"x\"}') AS x(a int, b text)",
    );
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][0], Value::Int(7));
    assert!(matches!(&got[0][1], Value::Text(s) if s == "x"));

    // json_ variant resolves the same way.
    let got = rows(
        &mut e,
        "SELECT string_agg(a::text, ',') FROM json_to_recordset('[{\"a\":10},{\"a\":20}]') AS x(a int)",
    );
    assert!(matches!(&got[0][0], Value::Text(s) if s == "10,20"));
}

#[test]
fn jsonb_each_in_select_list_is_composite_srf() {
    // v7.38 (read01, T15) — jsonb/json_each[_text] in the SELECT list (not FROM)
    // emit one composite `(key, value)` row per object member, like PG (which
    // renders each row as `(a,1)`). Covers no-FROM, _text (JSON null → SQL
    // NULL), over a real table, and NULL input. Oracle: live PG 18.4.
    use spg_storage::Value;
    let mut e = Engine::new();
    // no-FROM plain: value keeps jsonb rendering.
    let got = rows(&mut e, "SELECT jsonb_each('{\"a\": 1, \"b\": 2}'::jsonb)");
    assert_eq!(got.len(), 2);
    let comp = |v: &Value| match v {
        Value::Composite(f) => (render(&f[0].1), render(&f[1].1)),
        other => panic!("expected Composite, got {other:?}"),
    };
    assert_eq!(comp(&got[0][0]), (Some("a".into()), Some("1".into())));
    assert_eq!(comp(&got[1][0]), (Some("b".into()), Some("2".into())));
    // _text: string values unwrap, JSON null → SQL NULL.
    let got = rows(
        &mut e,
        "SELECT jsonb_each_text('{\"a\": \"x\", \"b\": null}'::jsonb)",
    );
    assert_eq!(comp(&got[0][0]), (Some("a".into()), Some("x".into())));
    assert_eq!(comp(&got[1][0]), (Some("b".into()), None));
    // Over a real table: one composite row per member per source row.
    e.execute("CREATE TABLE je(j jsonb)").unwrap();
    e.execute("INSERT INTO je VALUES ('{\"k\": 10}'), ('{\"m\": 20}')")
        .unwrap();
    let got = rows(&mut e, "SELECT jsonb_each(j) FROM je");
    assert_eq!(got.len(), 2);
    assert_eq!(comp(&got[0][0]), (Some("k".into()), Some("10".into())));
    assert_eq!(comp(&got[1][0]), (Some("m".into()), Some("20".into())));
    // NULL input → no rows.
    assert_eq!(rows(&mut e, "SELECT jsonb_each(NULL::jsonb)").len(), 0);
}
