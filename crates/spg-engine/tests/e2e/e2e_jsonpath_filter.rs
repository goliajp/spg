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
    assert_eq!(
        text(
            &mut e,
            "SELECT (jsonb_path_query_array('{\"a\":[1,2,3,4,5]}', '$.a[2 to 4]'))::text"
        ),
        "[3, 4, 5]"
    );
}

#[test]
fn jsonpath_filter_predicates() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_exists('{\"a\":5}', '$.a ? (@ > 3)')"
        ),
        "true"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_exists('{\"a\":5}', '$.a ? (@ > 9)')"
        ),
        "false"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT (jsonb_path_query_array('{\"items\":[{\"p\":10},{\"p\":25},{\"p\":5}]}', '$.items[*] ? (@.p > 8).p'))::text"
        ),
        "[10, 25]"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT (jsonb_path_query_array('[1,2,3,4]', '$[*] ? (@ >= 3)'))::text"
        ),
        "[3, 4]"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT (jsonb_path_query_array('[{\"n\":\"a\"},{\"n\":\"b\"}]', '$[*] ? (@.n == \"b\").n'))::text"
        ),
        "[\"b\"]"
    );
}

#[test]
fn jsonpath_at_question_with_filter() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT '{\"a\":1}'::jsonb @? '$.a ? (@ > 0)'"),
        "true"
    );
    assert_eq!(
        text(&mut e, "SELECT '{\"a\":1}'::jsonb @? '$.a ? (@ > 5)'"),
        "false"
    );
}

#[test]
fn jsonpath_at_at_toplevel_predicate() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT '{\"a\":5}'::jsonb @@ '$.a > 3'"),
        "true"
    );
    assert_eq!(
        text(&mut e, "SELECT '{\"a\":5}'::jsonb @@ '$.a < 3'"),
        "false"
    );
    assert_eq!(
        text(&mut e, "SELECT '{\"a\":5}'::jsonb @@ '$.a == 5'"),
        "true"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT '{\"a\":{\"b\":10}}'::jsonb @@ '$.a.b >= 10'"
        ),
        "true"
    );
    assert_eq!(
        text(&mut e, "SELECT jsonb_path_match('{\"a\":5}', '$.a > 3')"),
        "true"
    );
}

#[test]
fn jsonpath_filter_and_or() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT (jsonb_path_query_array('[{\"a\":5,\"b\":2},{\"a\":5,\"b\":9},{\"a\":1,\"b\":2}]', '$[*] ? (@.a == 5 && @.b < 5)'))::text"
        ),
        "[{\"a\": 5, \"b\": 2}]"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT (jsonb_path_query_array('[1,2,3,4,5]', '$[*] ? (@ < 2 || @ > 4)'))::text"
        ),
        "[1, 5]"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT (jsonb_path_query_array('[1,2,3,4,5,6]', '$[*] ? ((@ > 1 && @ < 3) || @ == 6)'))::text"
        ),
        "[2, 6]"
    );
}

// ── v7.39 jsonpath depth — differential-locked vs PG18.4 ──

#[test]
fn jsonpath_last_subscript() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_query_first('[1,2,3]', '$[last]')::text"
        ),
        "3"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_query_first('[1,2,3]', '$[last - 1]')::text"
        ),
        "2"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_query_array('[1,2,3,4]', '$[1 to last]')::text"
        ),
        "[2, 3, 4]"
    );
}

#[test]
fn jsonpath_numeric_item_methods() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_query_first('{\"n\":-5}', '$.n.abs()')::text"
        ),
        "5"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_query_first('{\"x\":4.7}', '$.x.floor()')::text"
        ),
        "4"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_query_first('{\"x\":4.2}', '$.x.ceiling()')::text"
        ),
        "5"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_query_first('{\"s\":\"1.5\"}', '$.s.double()')::text"
        ),
        "1.5"
    );
}

#[test]
fn jsonpath_string_predicates() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_query_first('{\"s\":\"abc\"}', '$.s ? (@ starts with \"ab\")')::text"
        ),
        "\"abc\""
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_query_first('{\"s\":\"a1\"}', '$.s ? (@ like_regex \"[a-z]\\d\")')::text"
        ),
        "\"a1\""
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_exists('{\"s\":\"zz\"}', '$.s ? (@ starts with \"ab\")')"
        ),
        "false"
    );
}

#[test]
fn jsonpath_null_comparison_and_exists() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_query_first('{\"a\":[1,null]}', '$.a[*] ? (@ == null)')::text"
        ),
        "null"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_query_first('{\"a\":1}', 'exists($.a)')::text"
        ),
        "true"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_query_first('{\"a\":1}', 'exists($.b)')::text"
        ),
        "false"
    );
}

#[test]
fn jsonpath_recursive_descent() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_query_first('{\"a\":{\"b\":{\"c\":1}}}', '$.**.c')::text"
        ),
        "1"
    );
}

#[test]
fn jsonpath_vars_third_argument() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_query_first('{\"a\":2}', '$.a ? (@ > $min)', '{\"min\":1}')::text"
        ),
        "2"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_exists('{\"a\":2}', '$.a ? (@ > $min)', '{\"min\":5}')"
        ),
        "false"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT jsonb_path_match('{\"a\":5}', '$.a > $lim', '{\"lim\":3}')"
        ),
        "true"
    );
}
