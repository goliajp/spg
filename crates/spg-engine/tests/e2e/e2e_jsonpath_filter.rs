//! v7.38 (read01, T8) — SQL/JSON path filter sublanguage: `[N to M]` ranges,
//! `? (@ <op> <lit>)` / `? (@.field <op> <lit>)` filters, and `.size()` /
//! `.type()` methods, plus `@?` with a filter. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            spg_storage::Value::Bool(b) => b.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn jsonpath_range() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT (jsonb_path_query_array('{\"a\":[1,2,3,4,5]}', '$.a[2 to 4]'))::text"), "[3, 4, 5]");
}

#[test]
fn jsonpath_filter_predicates() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT jsonb_path_exists('{\"a\":5}', '$.a ? (@ > 3)')"), "true");
    assert_eq!(text(&mut e, "SELECT jsonb_path_exists('{\"a\":5}', '$.a ? (@ > 9)')"), "false");
    assert_eq!(text(&mut e, "SELECT (jsonb_path_query_array('{\"items\":[{\"p\":10},{\"p\":25},{\"p\":5}]}', '$.items[*] ? (@.p > 8).p'))::text"), "[10, 25]");
    assert_eq!(text(&mut e, "SELECT (jsonb_path_query_array('[1,2,3,4]', '$[*] ? (@ >= 3)'))::text"), "[3, 4]");
    assert_eq!(text(&mut e, "SELECT (jsonb_path_query_array('[{\"n\":\"a\"},{\"n\":\"b\"}]', '$[*] ? (@.n == \"b\").n'))::text"), "[\"b\"]");
}

#[test]
fn jsonpath_at_question_with_filter() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT '{\"a\":1}'::jsonb @? '$.a ? (@ > 0)'"), "true");
    assert_eq!(text(&mut e, "SELECT '{\"a\":1}'::jsonb @? '$.a ? (@ > 5)'"), "false");
}

#[test]
fn jsonpath_at_at_toplevel_predicate() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT '{\"a\":5}'::jsonb @@ '$.a > 3'"), "true");
    assert_eq!(text(&mut e, "SELECT '{\"a\":5}'::jsonb @@ '$.a < 3'"), "false");
    assert_eq!(text(&mut e, "SELECT '{\"a\":5}'::jsonb @@ '$.a == 5'"), "true");
    assert_eq!(text(&mut e, "SELECT '{\"a\":{\"b\":10}}'::jsonb @@ '$.a.b >= 10'"), "true");
    assert_eq!(text(&mut e, "SELECT jsonb_path_match('{\"a\":5}', '$.a > 3')"), "true");
}
