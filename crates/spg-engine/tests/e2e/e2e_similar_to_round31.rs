//! v7.39 (read01 utils/adt, round 31) — the SIMILAR TO feature end to
//! end: operator (+ NOT / ESCAPE), the escape-double-quote substring
//! form, and similar_to_escape / similar_escape (PG-byte-identical
//! transform output). Byte-locked vs PG18.

use spg_engine::{Engine, QueryResult};

fn row_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn similar_to_operator() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT 'abc' SIMILAR TO 'a%', 'abc' SIMILAR TO 'a_c', 'abc' SIMILAR TO 'a', \
             'abc' SIMILAR TO '(a|b)%', 'abc' NOT SIMILAR TO 'z%'"
        ),
        vec!["true", "true", "false", "true", "true"]
    );
    // Regex metachars are literals in SIMILAR TO; classes pass through.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT 'a.c' SIMILAR TO 'a.c', 'abc' SIMILAR TO 'a.c', \
             'abc' SIMILAR TO 'a[bc]c', 'adc' SIMILAR TO 'a[^bc]c'"
        ),
        vec!["true", "false", "true", "true"]
    );
    // Custom escape; separators are inert in plain matching.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT 'a%c' SIMILAR TO 'a#%c' ESCAPE '#', \
             '50%' SIMILAR TO '%#\"50#\"%' ESCAPE '#'"
        ),
        vec!["true", "true"]
    );
    let err = e
        .execute("SELECT 'abc' SIMILAR TO 'ab' ESCAPE 'xy'")
        .unwrap_err();
    assert!(format!("{err}").contains("invalid escape string"), "{err}");
}

#[test]
fn substring_similar_three_part() {
    let mut e = Engine::new();
    // part1 matches as little as possible; part2 (captured) as much.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT substring('foobar' similar '%#\"o_#\"%' escape '#'), \
             substring('foobar' similar 'f#\"o+#\"%' escape '#')"
        ),
        vec!["oo", "oo"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT substring('foobar' similar 'z#\"o_#\"%' escape '#')"
        ),
        vec!["NULL"]
    );
}

#[test]
fn escape_transform_functions() {
    let mut e = Engine::new();
    // Byte-identical to PG's similar_escape output.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT similar_to_escape('a%b_c'), similar_to_escape('a#%b', '#'), \
             similar_escape('a%b', NULL), similar_to_escape('%#\"50#\"%', '#')"
        ),
        vec![
            "^(?:a.*b.c)$",
            "^(?:a\\%b)$",
            "^(?:a.*b)$",
            "^(?:.*){1,1}?(50){1,1}(?:.*)$"
        ]
    );
}
