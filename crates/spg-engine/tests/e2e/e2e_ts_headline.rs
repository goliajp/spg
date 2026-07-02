//! v7.37.17 (17.6 siblings) — ts_headline([config,] doc, query
//! [, options]): match highlighting for search UIs.

use spg_engine::{Engine, QueryResult};

fn first_text(e: &mut Engine, sql: &str) -> String {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn basic_highlight_default_selectors() {
    let mut e = Engine::new();
    let got = first_text(
        &mut e,
        "SELECT ts_headline('the quick brown fox', to_tsquery('fox'))",
    );
    assert_eq!(got, "the quick brown <b>fox</b>");
}

#[test]
fn english_config_stems_document_words() {
    let mut e = Engine::new();
    // Query lexeme 'jump' (stemmed from 'jumping'); document word
    // 'jumps' stems to the same lexeme, so it highlights.
    let got = first_text(
        &mut e,
        "SELECT ts_headline('english', 'the fox jumps high', \
         to_tsquery('english', 'jumping'))",
    );
    assert_eq!(got, "the fox <b>jumps</b> high");
}

#[test]
fn options_override_selectors() {
    let mut e = Engine::new();
    let got = first_text(
        &mut e,
        "SELECT ts_headline('a cat sat', to_tsquery('cat'), \
         'StartSel=<em>, StopSel=</em>')",
    );
    assert_eq!(got, "a <em>cat</em> sat");
}

#[test]
fn not_terms_are_not_highlighted() {
    let mut e = Engine::new();
    let got = first_text(
        &mut e,
        "SELECT ts_headline('cat and dog', to_tsquery('cat & !dog'))",
    );
    assert_eq!(got, "<b>cat</b> and dog");
}

#[test]
fn multiple_occurrences_and_case_preserved() {
    let mut e = Engine::new();
    let got = first_text(
        &mut e,
        "SELECT ts_headline('Cat sees cat', to_tsquery('cat'))",
    );
    assert_eq!(got, "<b>Cat</b> sees <b>cat</b>");
}
