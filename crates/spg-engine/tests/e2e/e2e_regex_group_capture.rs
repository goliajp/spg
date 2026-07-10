//! v7.38 (read01, T7) — regex group capture: regexp_matches returns the groups,
//! regexp_replace substitutes `\1`..`\9` / `\&`, and substring(FROM pattern)
//! returns the first group. A capture-aware matcher (parallel to the hot LIKE
//! matcher) tracks group spans with a journal-based backtrack undo. Oracle: PG18.4.

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
fn regexp_matches_returns_groups() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT (regexp_matches('John Smith', '(\\w+) (\\w+)'))::text"
        ),
        "{John,Smith}"
    );
    // No groups → whole match.
    assert_eq!(
        text(&mut e, "SELECT (regexp_matches('abc123', '[a-z]+'))::text"),
        "{abc}"
    );
    // A non-participating alternation branch is a NULL element (journal undo).
    assert_eq!(
        text(&mut e, "SELECT (regexp_matches('b', '(a)|(b)'))::text"),
        "{NULL,b}"
    );
    // A quantified group keeps its LAST repetition.
    assert_eq!(
        text(&mut e, "SELECT (regexp_matches('aaab', '(a)*b'))::text"),
        "{a}"
    );
}

#[test]
fn regexp_replace_backreferences() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT regexp_replace('John Smith', '(\\w+) (\\w+)', '\\2 \\1')"
        ),
        "Smith John"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT regexp_replace('2024-06-15', '(\\d+)-(\\d+)-(\\d+)', '\\3/\\2/\\1')"
        ),
        "15/06/2024"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT regexp_replace('a1b2c3', '([a-z])(\\d)', '\\2\\1', 'g')"
        ),
        "1a2b3c"
    );
    // `\&` = whole match; `\\` = literal backslash.
    assert_eq!(
        text(&mut e, "SELECT regexp_replace('abc', 'b', '[\\&]')"),
        "a[b]c"
    );
    assert_eq!(
        text(&mut e, "SELECT regexp_replace('x', 'x', 'a\\\\b')"),
        "a\\b"
    );
}

#[test]
fn substring_from_pattern_returns_first_group() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT substring('John Smith' from '(\\w+)')"),
        "John"
    );
    assert_eq!(
        text(&mut e, "SELECT substring('2024-06-15' from '\\d+-(\\d+)')"),
        "06"
    );
    // No group → whole match; no match → NULL.
    assert_eq!(
        text(&mut e, "SELECT substring('abc123' from '[0-9]+')"),
        "123"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT COALESCE(substring('abc' from '(\\d+)'), '<NULL>')"
        ),
        "<NULL>"
    );
}
