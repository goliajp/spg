//! v7.38 (read01) — JSON/JSONB subscripting (PG 14+): `j['key']`, `j[0]`, and
//! chained `j['a']['b']` do object/array access identical to `->` (text key →
//! object field, integer → 0-based array element). Real array subscripting
//! stays 1-based and unaffected. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            spg_storage::Value::Null => "<NULL>".into(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn jsonb_subscript_object_and_array() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT (('{\"a\":{\"b\":1}}'::jsonb)['a']['b'])::text"), "1");
    assert_eq!(text(&mut e, "SELECT (('[10,20,30]'::jsonb)[1])::text"), "20"); // 0-based
    assert_eq!(text(&mut e, "SELECT (('{\"a\":[5,6,7]}'::jsonb)['a'][2])::text"), "7");
    assert_eq!(text(&mut e, "SELECT (('{\"a\":1}'::jsonb)['x'])::text"), "<NULL>");
}

#[test]
fn real_array_subscript_still_one_based() {
    let mut e = Engine::new();
    // Regression: a genuine SQL array is still 1-based.
    assert_eq!(text(&mut e, "SELECT ((ARRAY[10,20,30])[2])::text"), "20");
    assert_eq!(text(&mut e, "SELECT ((ARRAY[10,20,30])[1])::text"), "10");
}
