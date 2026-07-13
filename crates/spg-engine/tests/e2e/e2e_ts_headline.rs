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

// v7.39 (FTS mark_hl_fragments 研读轮) — the MaxFragments selector
// follows PG's cover/greedy/stretch/shrink algorithm; every expected
// string below is the live PG18 oracle's output byte-for-byte.
#[test]
fn fragment_selector_matches_pg_algorithm() {
    let mut e = Engine::new();
    const DOC: &str = "The quick brown fox jumps over the lazy dog while the cat \
watches the fox and the dog from a tall tree near the river bank where fish swim \
under the old wooden bridge every sunny morning";
    // Single-term: asymmetric stretch (left <= half the remainder,
    // right takes the rest), bad endpoints shrunk.
    let got = first_text(
        &mut e,
        &format!(
            "SELECT ts_headline('english', '{DOC}', to_tsquery('english','fox'), \
             'MinWords=2, MaxFragments=1, MaxWords=8')"
        ),
    );
    assert_eq!(got, "quick brown <b>fox</b> jumps over the lazy");
    // Two fragments in document order, greedy by interesting words.
    let got = first_text(
        &mut e,
        &format!(
            "SELECT ts_headline('english', '{DOC}', to_tsquery('english','fox'), \
             'MinWords=2, MaxFragments=2, MaxWords=6')"
        ),
    );
    assert_eq!(got, "quick brown <b>fox</b> jumps over ... watches the <b>fox</b>");
    // AND cover spans both terms; split into <= MaxWords fragments.
    let got = first_text(
        &mut e,
        &format!(
            "SELECT ts_headline('english', '{DOC}', \
             to_tsquery('english','fox & bridge'), \
             'MinWords=2, MaxFragments=2, MaxWords=10')"
        ),
    );
    assert_eq!(
        got,
        "watches the <b>fox</b> and the dog from a tall tree ... \
under the old wooden <b>bridge</b> every sunny morning"
    );
    // ShortWord controls endpoint shrinking.
    let got = first_text(
        &mut e,
        "SELECT ts_headline('english', 'one two three four five six seven eight \
nine ten matchword eleven twelve thirteen fourteen fifteen', \
         to_tsquery('simple','matchword'), \
         'MinWords=2, MaxFragments=1, MaxWords=5, ShortWord=5')",
    );
    assert_eq!(got, "<b>matchword</b> eleven twelve thirteen fourteen");
    // Unmatched LONG document: first MinWords words (fragment mode's
    // only use of MinWords) — same in window mode.
    let got = first_text(
        &mut e,
        "SELECT ts_headline('english', 'The quick brown fox jumps over the lazy dog', \
         to_tsquery('english','absentterm'), 'MinWords=4, MaxFragments=2, MaxWords=6')",
    );
    assert_eq!(got, "The quick brown fox");
    let got = first_text(
        &mut e,
        "SELECT ts_headline('english', 'The quick brown fox jumps over the lazy dog \
while the cat watches', to_tsquery('english','absentterm'), 'MinWords=4, MaxWords=6')",
    );
    assert_eq!(got, "The quick brown fox");
}

// v7.39 (read01, ts_headline validation) — PG validates the option list
// instead of silently defaulting. Error texts differential-locked vs
// PG18 (22023 for range/unknown, 22P02 for a bad integer, 42601 for a
// malformed pair); HighlightAll's boolean reader is lenient like PG's.
#[test]
fn option_validation_matches_pg() {
    let mut e = Engine::new();
    let err = |e: &mut Engine, sql: &str| -> String {
        format!("{}", e.execute(sql).unwrap_err())
    };
    assert!(
        err(
            &mut e,
            "SELECT ts_headline('a b c', 'a'::tsquery, 'MinWords=10, MaxWords=5')"
        )
        .contains("MinWords must be less than MaxWords")
    );
    assert!(
        err(&mut e, "SELECT ts_headline('a b c', 'a'::tsquery, 'MinWords=0')")
            .contains("MinWords must be positive")
    );
    assert!(
        err(&mut e, "SELECT ts_headline('a b c', 'a'::tsquery, 'ShortWord=-1')")
            .contains("ShortWord must be >= 0")
    );
    assert!(
        err(
            &mut e,
            "SELECT ts_headline('a b c', 'a'::tsquery, 'MaxFragments=-1')"
        )
        .contains("MaxFragments must be >= 0")
    );
    assert!(
        err(&mut e, "SELECT ts_headline('a b c', 'a'::tsquery, 'Bogus=1')")
            .contains("unrecognized headline parameter: \"Bogus\"")
    );
    assert!(
        err(&mut e, "SELECT ts_headline('a b c', 'a'::tsquery, 'MaxWords=zzz')")
            .contains("invalid input syntax for type integer: \"zzz\"")
    );
    assert!(
        err(&mut e, "SELECT ts_headline('a b c', 'a'::tsquery, 'StartSel=')")
            .contains("invalid parameter list format: \"StartSel=\"")
    );
    // Fragment mode validates the same set.
    assert!(
        err(
            &mut e,
            "SELECT ts_headline('a b c', 'a'::tsquery, 'MaxFragments=2, MinWords=0')"
        )
        .contains("MinWords must be positive")
    );
    // Valid options still work; HighlightAll reads 1/on/t/true/y/yes as
    // true and anything else as false without erroring (PG).
    let ok = |e: &mut Engine, sql: &str| match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(
        ok(
            &mut e,
            "SELECT ts_headline('a b c', 'a'::tsquery, 'StartSel=\"[\", StopSel=\"]\"')"
        ),
        "[a] b c"
    );
    assert_eq!(
        ok(
            &mut e,
            "SELECT ts_headline('a b c', 'a'::tsquery, 'HighlightAll=zzz')"
        ),
        "<b>a</b> b c"
    );
}
