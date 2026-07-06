//! v7.38 (read01 P6.15) — PG's ARE is non-newline-sensitive by default, so
//! `.` matches any character including a newline (no `s`/DOTALL flag needed).
//! Oracle values from live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn b(e: &mut Engine, sql: &str) -> bool {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => matches!(rows[0].values[0], spg_storage::Value::Bool(true)),
        _ => panic!("expected rows"),
    }
}

#[test]
fn dot_matches_newline_by_default() {
    let mut e = Engine::new();
    assert!(b(&mut e, "SELECT regexp_match(E'a\nb', 'a.b') IS NOT NULL"));
    // Explicit (?s) is the same as the default in PG.
    assert!(b(&mut e, "SELECT regexp_match(E'a\nb', '(?s)a.b') IS NOT NULL"));
    // `~` / anchored short-circuit path.
    assert!(b(&mut e, "SELECT E'two\nlines' ~ '^.*$'"));
    // Ordinary single-line matches are unaffected.
    assert!(b(&mut e, "SELECT regexp_match('axb', 'a.b') IS NOT NULL"));
}

#[test]
fn dot_star_spans_multiline() {
    let mut e = Engine::new();
    assert_eq!(
        match e.execute("SELECT regexp_replace(E'a\nb\nc', '.', 'X', 'g')").unwrap() {
            QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
                spg_storage::Value::Text(s) => s.to_string(),
                v => panic!("{v:?}"),
            },
            _ => panic!(),
        },
        // Every character, newlines included, is replaced.
        "XXXXX"
    );
}
