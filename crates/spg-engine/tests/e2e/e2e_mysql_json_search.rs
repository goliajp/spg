//! v7.37.17 (17.6 siblings) — MySQL json_search + json_value close
//! the MySQL JSON surface.

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
        spg_storage::Value::Json(s) => s.to_string(),
        other => panic!("expected Text/Json, got {other:?}"),
    }
}

// MySQL doc corpus: '["abc", [{"k": "10"}, "def"], {"x":"abc"},
// {"y":"bcd"}]'.
const DOC: &str = "'[\"abc\", [{\"k\": \"10\"}, \"def\"], {\"x\":\"abc\"}, {\"y\":\"bcd\"}]'";

#[test]
fn json_search_one_and_all() {
    let mut e = Engine::new();
    // MySQL doc vector: JSON_SEARCH(doc, 'one', 'abc') → "$[0]".
    assert_eq!(
        text(&first(
            &mut e,
            &format!("SELECT json_search({DOC}, 'one', 'abc')")
        )),
        "\"$[0]\""
    );
    // MySQL doc vector: JSON_SEARCH(doc, 'all', 'abc')
    // → ["$[0]", "$[2].x"].
    assert_eq!(
        text(&first(
            &mut e,
            &format!("SELECT json_search({DOC}, 'all', 'abc')")
        )),
        "[\"$[0]\",\"$[2].x\"]"
    );
    // No match → NULL.
    assert!(matches!(
        first(&mut e, &format!("SELECT json_search({DOC}, 'all', 'ghi')")),
        spg_storage::Value::Null
    ));
}

#[test]
fn json_search_like_wildcards_and_start_path() {
    let mut e = Engine::new();
    // MySQL doc vector: JSON_SEARCH(doc, 'all', '%b%', NULL, '$[3]')
    // → "$[3].y".
    assert_eq!(
        text(&first(
            &mut e,
            &format!("SELECT json_search({DOC}, 'all', '%b%', NULL, '$[3]')")
        )),
        "\"$[3].y\""
    );
    // MySQL doc vector: JSON_SEARCH(doc, 'all', '10') → "$[1][0].k".
    assert_eq!(
        text(&first(
            &mut e,
            &format!("SELECT json_search({DOC}, 'all', '10')")
        )),
        "\"$[1][0].k\""
    );
}

#[test]
fn json_value_scalar_and_miss() {
    let mut e = Engine::new();
    // MySQL doc vector shape: JSON_VALUE('{"fname": "Joe"}',
    // '$.fname') → Joe (unquoted).
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT json_value('{\"fname\": \"Joe\", \"lname\": \"Palmer\"}', '$.fname')"
        )),
        "Joe"
    );
    // Numbers come back as their lexeme.
    assert_eq!(
        text(&first(&mut e, "SELECT json_value('{\"n\": 42}', '$.n')")),
        "42"
    );
    // Miss → NULL.
    assert!(matches!(
        first(&mut e, "SELECT json_value('{\"a\": 1}', '$.zzz')"),
        spg_storage::Value::Null
    ));
}
