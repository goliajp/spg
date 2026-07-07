//! v7.38 (read01 sweep) — json_object / jsonb_object accept untyped `'{a,b}'`
//! text-literal arguments (PG coerces them to text[]), not just ARRAY[...] /
//! ::text[]. Oracle behaviour from live PG 18.4 (content, whitespace aside).

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn json_object_accepts_text_array_literals() {
    let mut e = Engine::new();
    // Two-array text-literal form.
    assert_eq!(
        text(&mut e, "SELECT json_object('{a,b}', '{1,2}')::text"),
        r#"{"a":"1","b":"2"}"#
    );
    // Flat single-array text-literal form.
    assert_eq!(
        text(&mut e, "SELECT json_object('{a,1,b,2}')::text"),
        r#"{"a":"1","b":"2"}"#
    );
    // jsonb_object too.
    assert_eq!(
        text(&mut e, "SELECT jsonb_object('{a,b}', '{1,2}')::text"),
        r#"{"a":"1","b":"2"}"#
    );
    // The ARRAY[...] spelling still works.
    assert_eq!(
        text(&mut e, "SELECT json_object(ARRAY['a','b'], ARRAY['1','2'])::text"),
        r#"{"a":"1","b":"2"}"#
    );
}
