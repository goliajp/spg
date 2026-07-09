//! v7.17.0 Phase 3.7 — PG regex function family.
//! regexp_matches / regexp_replace / regexp_split_to_array.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

fn unwrap_text_array(v: &Value) -> Vec<Option<String>> {
    match v {
        Value::TextArray(a) => a.clone(),
        other => panic!("expected TextArray, got {other:?}"),
    }
}

// ── regexp_matches ──────────────────────────────────────────────────

#[test]
fn matches_simple_literal() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT regexp_matches('hello world', 'world')")
            .unwrap(),
    );
    let a = unwrap_text_array(&r[0][0]);
    assert_eq!(a, vec![Some("world".into())]);
}

#[test]
fn matches_digit_shortcut() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r"SELECT regexp_matches('abc123def', '\d+')")
            .unwrap(),
    );
    let a = unwrap_text_array(&r[0][0]);
    assert_eq!(a, vec![Some("123".into())]);
}

#[test]
fn matches_global_flag() {
    // v7.38 (read01, T15) — regexp_matches is set-returning: the `g` flag emits
    // one ROW per match (each a text[]), not one flat array. No capture group →
    // each row is the whole match. Oracle: live PG 18.4.
    let mut e = Engine::new();
    let r = rows(
        e.execute(r"SELECT regexp_matches('a1b22c333', '\d+', 'g')")
            .unwrap(),
    );
    assert_eq!(r.len(), 3);
    assert_eq!(unwrap_text_array(&r[0][0]), vec![Some("1".into())]);
    assert_eq!(unwrap_text_array(&r[1][0]), vec![Some("22".into())]);
    assert_eq!(unwrap_text_array(&r[2][0]), vec![Some("333".into())]);
}

#[test]
fn matches_no_match_empty() {
    // No match → zero rows (PG), not one row of an empty array.
    let mut e = Engine::new();
    let r = rows(e.execute(r"SELECT regexp_matches('hello', '\d+')").unwrap());
    assert!(r.is_empty());
}

#[test]
fn matches_null_propagates() {
    // A NULL argument yields zero rows (PG), not one NULL row.
    let mut e = Engine::new();
    let r = rows(e.execute(r"SELECT regexp_matches(NULL, '\d+')").unwrap());
    assert!(r.is_empty());
}

#[test]
fn matches_groups_and_sibling_broadcast() {
    // v7.38 (read01, T15) — capture groups form each row's text[]; a sibling
    // scalar column repeats per match row. Oracle: live PG 18.4.
    let mut e = Engine::new();
    let r = rows(
        e.execute(r"SELECT 'x', regexp_matches('a1b2', '(\w)(\d)', 'g')")
            .unwrap(),
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("x".into()));
    assert_eq!(unwrap_text_array(&r[0][1]), vec![Some("a".into()), Some("1".into())]);
    assert_eq!(r[1][0], Value::Text("x".into()));
    assert_eq!(unwrap_text_array(&r[1][1]), vec![Some("b".into()), Some("2".into())]);
}

// ── regexp_replace ─────────────────────────────────────────────────

#[test]
fn replace_single_first_match() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT regexp_replace('hello world', 'world', 'PG')")
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::text("hello PG"));
}

#[test]
fn replace_first_match_only_by_default() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT regexp_replace('a b a b', 'a', 'X')")
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::text("X b a b"));
}

#[test]
fn replace_global_flag() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT regexp_replace('a b a b', 'a', 'X', 'g')")
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::text("X b X b"));
}

#[test]
fn replace_with_character_class() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r"SELECT regexp_replace('Hello, World!', '[^a-zA-Z0-9]', '-', 'g')")
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::text("Hello--World-"));
}

#[test]
fn replace_digit_with_hash() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r"SELECT regexp_replace('order #1234', '\d+', '#', 'g')")
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::text("order ##"));
}

#[test]
fn replace_no_match_unchanged() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r"SELECT regexp_replace('hello', '\d+', 'X')")
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::text("hello"));
}

// ── regexp_split_to_array ──────────────────────────────────────────

#[test]
fn split_on_comma() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT regexp_split_to_array('a,b,c', ',')")
            .unwrap(),
    );
    let a = unwrap_text_array(&r[0][0]);
    assert_eq!(
        a,
        vec![Some("a".into()), Some("b".into()), Some("c".into())]
    );
}

#[test]
fn split_on_whitespace_pattern() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r"SELECT regexp_split_to_array('one two   three', '\s+')")
            .unwrap(),
    );
    let a = unwrap_text_array(&r[0][0]);
    assert_eq!(
        a,
        vec![Some("one".into()), Some("two".into()), Some("three".into())]
    );
}

#[test]
fn split_no_delimiter_returns_single_element() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT regexp_split_to_array('abc', ',')")
            .unwrap(),
    );
    let a = unwrap_text_array(&r[0][0]);
    assert_eq!(a, vec![Some("abc".into())]);
}

#[test]
fn split_on_character_class() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r"SELECT regexp_split_to_array('a1b2c3', '[0-9]')")
            .unwrap(),
    );
    let a = unwrap_text_array(&r[0][0]);
    assert_eq!(
        a,
        vec![
            Some("a".into()),
            Some("b".into()),
            Some("c".into()),
            Some(String::new()),
        ]
    );
}

// ── error paths ────────────────────────────────────────────────────

#[test]
fn invalid_pattern_errors_cleanly() {
    let mut e = Engine::new();
    let r = e.execute(r"SELECT regexp_matches('x', '[unterminated')");
    assert!(r.is_err());
}

#[test]
fn from_regexp_matches_is_a_row_source() {
    // v7.38 (T15) — `FROM regexp_matches(...)` yields one text[] row per match
    // (it can't desugar to unnest, which would flatten the group array). The
    // column is named `regexp_matches`, overridable by an alias, and the array
    // is usable downstream via subscript. Oracle: live PG 18.4.
    use spg_storage::Value;
    let mut e = Engine::new();

    // Row count + values.
    let r = e
        .execute(r"SELECT * FROM regexp_matches('a1b2', '(\w)(\d)', 'g')")
        .unwrap();
    let QueryResult::Rows { columns, rows: got_rows } = r else {
        panic!("rows")
    };
    assert_eq!(columns[0].name, "regexp_matches");
    assert_eq!(got_rows.len(), 2);
    assert_eq!(unwrap_text_array(&got_rows[0].values[0]), vec![Some("a".into()), Some("1".into())]);
    assert_eq!(unwrap_text_array(&got_rows[1].values[0]), vec![Some("b".into()), Some("2".into())]);

    // No `g` flag → the single first match.
    let r = rows(e.execute(r"SELECT * FROM regexp_matches('abc', '(a)(b)')").unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!(unwrap_text_array(&r[0][0]), vec![Some("a".into()), Some("b".into())]);

    // Column alias + subscript access to the groups.
    let r = rows(
        e.execute(r"SELECT g[1], g[2] FROM regexp_matches('a1b2', '(\w)(\d)', 'g') AS t(g)")
            .unwrap(),
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("a".into()));
    assert_eq!(r[0][1], Value::Text("1".into()));
    assert_eq!(r[1][0], Value::Text("b".into()));

    // count(*) composes.
    let r = rows(e.execute(r"SELECT count(*) FROM regexp_matches('a1b2', '(\w)(\d)', 'g')").unwrap());
    assert!(matches!(r[0][0], Value::Int(2) | Value::BigInt(2)));
}
