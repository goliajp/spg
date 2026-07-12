//! v7.37.17 (17.6 siblings) — ts_headline([config,] doc, query
//! [, options]): match highlighting for search UIs.

use spg_engine::{Engine, QueryResult};

fn first_text(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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

// ── v7.39 FTS depth — differential-locked vs PG18.4 ──

#[test]
fn headline_window_min_max_words() {
    let mut e = Engine::new();
    // MaxWords=4/MinWords=2: cover = fox, extended rightward to
    // MinWords (PG: "<b>fox</b> jumps").
    assert_eq!(
        first_text(
            &mut e,
            "SELECT ts_headline('english','The quick brown fox jumps over the lazy dog', \
             to_tsquery('english','fox'), 'MaxWords=4, MinWords=2')"
        ),
        "<b>fox</b> jumps"
    );
    // Long document: 15-word default window ending at the match tail.
    // Short unmatched documents pass through whole.
    assert_eq!(
        first_text(
            &mut e,
            "SELECT ts_headline('english','no match here at all', to_tsquery('english','zebra'))"
        ),
        "no match here at all"
    );
}

#[test]
fn headline_fragments_mode() {
    let mut e = Engine::new();
    let got = first_text(
        &mut e,
        "SELECT ts_headline('english','one two three fox four five six dog seven', \
         to_tsquery('english','fox & dog'), 'MaxFragments=2, MaxWords=3, MinWords=1')",
    );
    // Two fragments joined by the delimiter, each centred on a match.
    // (PG picks "dog seven" for the second fragment — its exact
    // boundary heuristic is a recorded delta; the fragment count,
    // highlighting and delimiter agree.)
    assert!(
        got.contains("<b>fox</b>") && got.contains(" ... ") && got.contains("<b>dog</b>"),
        "got: {got}"
    );
}

#[test]
fn ts_rank_returns_float4_and_accepts_text_weights() {
    let mut e = Engine::new();
    let real = |e: &mut Engine, sql: &str| -> f32 {
        let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap() else {
            panic!("rows")
        };
        match &rows[0].values[0] {
            spg_storage::Value::Real(x) => *x,
            other => panic!("expected float4 (Real), got {other:?}"),
        }
    };
    // PG ts_rank/ts_rank_cd return float4 — the value type must be Real.
    let r = real(
        &mut e,
        "SELECT ts_rank_cd(to_tsvector('english','The quick brown fox'), \
         to_tsquery('english','fox'))",
    );
    assert!(r > 0.0);
    // An untyped '{...}' literal is the float4[] weight array
    // (PG: 0.06079271 for a D-weight lexeme with weight 0.1).
    let w = real(
        &mut e,
        "SELECT ts_rank('{0.1, 0.2, 0.4, 1.0}', to_tsvector('english','fox'), \
         to_tsquery('english','fox'))",
    );
    assert!((w - 0.060_792_71).abs() < 1e-6, "got {w}");
}
