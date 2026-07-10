//! v7.38 (read01, T9) — ROW(...) is a first-class composite value, so
//! row_to_json / to_json emit a JSON object keyed by field name (f1..fN), the
//! text form is PG's record_out `(a,b)`, and composites nest. Oracle: PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            spg_storage::Value::Json(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn row_to_json_builds_object() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT row_to_json(row(1,'a'))"),
        "{\"f1\":1,\"f2\":\"a\"}"
    );
    assert_eq!(
        text(&mut e, "SELECT row_to_json(row(1,'a',true))"),
        "{\"f1\":1,\"f2\":\"a\",\"f3\":true}"
    );
    assert_eq!(
        text(&mut e, "SELECT to_json(row(1,'a'))"),
        "{\"f1\":1,\"f2\":\"a\"}"
    );
    // NULL field → JSON null.
    assert_eq!(
        text(&mut e, "SELECT row_to_json(row(1,NULL,'x'))"),
        "{\"f1\":1,\"f2\":null,\"f3\":\"x\"}"
    );
    // Nested composite → nested object.
    assert_eq!(
        text(&mut e, "SELECT row_to_json(row(1, row(2,3)))"),
        "{\"f1\":1,\"f2\":{\"f1\":2,\"f2\":3}}"
    );
    // to_jsonb canonicalises (spaces after ':' and ',').
    assert_eq!(
        text(&mut e, "SELECT (to_jsonb(row(5,'q')))::text"),
        "{\"f1\": 5, \"f2\": \"q\"}"
    );
}

#[test]
fn composite_text_form() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT (row(1,'a',true))::text"), "(1,a,t)");
    assert_eq!(
        text(&mut e, "SELECT (row(1,NULL,'x,y'))::text"),
        "(1,,\"x,y\")"
    );
    assert!(matches!(
        e.execute("SELECT row(1,'a')").unwrap(),
        QueryResult::Rows { .. }
    ));
}
