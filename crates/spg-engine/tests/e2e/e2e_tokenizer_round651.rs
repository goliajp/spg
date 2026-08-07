//! v7.39 (round 651) — the tokenizer agreed with PG on four of its
//! twenty-three token shapes.
//!
//! Round 650 stopped at the catalogs and recorded the token-type model
//! as the thing behind them. Measuring what SPG's parser actually
//! produced turned out to matter far more than the catalogs did: it was
//! sixteen lines — "split on anything not alphanumeric" — and across
//! PG's token types it agreed on **4 of 17** probes and differed on 13.
//!
//! This is not a catalog gap, it is a SEARCH-QUALITY gap. A customer
//! indexing text got different rows back than PG would give:
//!
//!   * `user@example.com` indexed as `user`, `example`, `com`, so a
//!     search for the address matched nothing
//!   * `3.14` became `3` and `14`; `1.2.3` became `1`, `2`, `3`
//!   * `-42` lost its sign
//!   * `fox-run` lost the compound, keeping only the parts
//!   * `<b>x</b>` put the TAG NAME into the index, twice — markup
//!     polluting search results — and `&amp;` indexed `amp`
//!
//! The parser is typed now, the way PG's is, and the type picks the
//! dictionary — which is what `pg_ts_config_map` records, so the
//! catalog is generated from the same function the indexer calls and
//! cannot drift from it. `ts_token_type` and `ts_debug` are projections
//! of the same model.
//!
//! Two rules here were measured rather than reasoned from the type
//! names, and both had already been got wrong on the first attempt:
//!
//!   * a URL emits protocol + url + host + url_path, but ONLY when the
//!     part before the first `/` looks like a host. `ts_debug` on PG18:
//!     `http://example.com/a/b` splits four ways, `http://x.y/z` is a
//!     single `file`, and `http://x.co/z` splits again — so what
//!     decides it is the last label's length, not the presence of `://`.
//!   * the token keeps the text the PARSER saw, not the folded form:
//!     `ts_debug`'s `token` column shows `The` while its `lexemes` shows
//!     `{}`. Lowercasing belongs to the dictionary.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn one(e: &mut Engine, sql: &str) -> String {
    rows(e, sql).join(",")
}

/// Every shape, against the PG18 reading. These are the thirteen that
/// differed plus the four that did not.
#[test]
fn round651_the_token_shapes_match_pg() {
    let mut e = Engine::new();
    for (input, want) in [
        ("hello", "'hello':1"),
        ("naïve", "'naïve':1"),
        ("abc123", "'abc123':1"),
        ("user@example.com", "'user@example.com':1"),
        ("example.com", "'example.com':1"),
        ("1.5e10", "'1.5e10':1"),
        ("1.2.3", "'1.2.3':1"),
        ("3.14", "'3.14':1"),
        ("-42", "'-42':1"),
        ("42", "'42':1"),
        ("/usr/local/bin", "'/usr/local/bin':1"),
    ] {
        assert_eq!(
            one(&mut e, &format!("SELECT to_tsvector('simple', '{input}')")),
            want,
            "{input}"
        );
    }
}

/// A hyphenated word is the compound AND its parts, all three.
#[test]
fn round651_a_hyphenated_word_keeps_its_compound() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT to_tsvector('simple', 'fox-run')"),
        "'fox':2 'fox-run':1 'run':3"
    );
    assert_eq!(
        one(&mut e, "SELECT to_tsvector('simple', 'abc123-def')"),
        "'abc123':2 'abc123-def':1 'def':3"
    );
}

/// Markup is recognised so that it can be thrown away. This is the one
/// that was actively polluting the index.
#[test]
fn round651_markup_does_not_reach_the_index() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT to_tsvector('simple', '<b>x</b>')"),
        "'x':1"
    );
    assert_eq!(one(&mut e, "SELECT to_tsvector('simple', '&amp;')"), "");
    // …and a whole sentence of it.
    assert_eq!(
        one(
            &mut e,
            "SELECT to_tsvector('simple', '<p class=\"c\">hello</p> &nbsp; world')"
        ),
        "'hello':1 'world':2"
    );
}

/// The measured URL rule: a host-looking head splits, anything else is
/// one file token.
#[test]
fn round651_a_url_splits_only_when_its_head_is_a_host() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT to_tsvector('simple', 'http://example.com/a/b')"
        ),
        "'/a/b':3 'example.com':2 'example.com/a/b':1"
    );
    // A one-letter last label is not a host; PG calls the whole thing a
    // file and so does this.
    assert_eq!(
        one(&mut e, "SELECT to_tsvector('simple', 'http://x.y/z')"),
        "'x.y/z':1"
    );
    assert_eq!(
        one(&mut e, "SELECT to_tsvector('simple', 'http://x.co/z')"),
        "'/z':3 'x.co':2 'x.co/z':1"
    );
}

#[test]
fn round651_ts_token_type_lists_the_parser_it_has() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ts_token_type('default')"),
        "23"
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT tokid, alias, description FROM ts_token_type('default') \
             WHERE tokid IN (1, 13, 21) ORDER BY tokid"
        ),
        vec![
            "1|asciiword|Word, all ASCII",
            "13|tag|XML tag",
            "21|int|Signed integer",
        ]
    );
    // SPG has the one parser, and says so about any other name.
    assert!(e.execute("SELECT * FROM ts_token_type('nosuch')").is_err());
}

/// The map is generated from the same function the indexer calls, so
/// these rows ARE the behaviour rather than a description of it.
#[test]
fn round651_the_config_map_is_the_indexers_own_rule() {
    let mut e = Engine::new();
    // Nineteen of twenty-three per configuration; the four left out are
    // blank, tag, protocol and entity — the ones that yield no lexeme.
    assert_eq!(one(&mut e, "SELECT count(*) FROM pg_ts_config_map"), "38");
    assert_eq!(
        rows(
            &mut e,
            "SELECT maptokentype FROM pg_ts_config_map \
             WHERE mapcfg = 13248 AND maptokentype IN (12,13,14,23)"
        ),
        Vec::<String>::new()
    );
    // Under `english` the stemmer takes only the word-shaped types.
    assert_eq!(
        rows(
            &mut e,
            "SELECT maptokentype FROM pg_ts_config_map \
             WHERE mapcfg = 13248 AND mapdict = 13247 ORDER BY maptokentype"
        ),
        vec!["1", "2", "10", "11", "16", "17"]
    );
}

#[test]
fn round651_ts_debug_reports_the_pipeline_that_runs() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT alias, token, dictionary, lexemes FROM \
             ts_debug('english', 'The quick 42 fox-run') ORDER BY token"
        ),
        vec![
            "uint|42|simple|{42}",
            // The token keeps the case the parser saw; the dictionary
            // folds it, which is why the lexeme list is empty.
            "asciiword|The|english_stem|{}",
            "hword_asciipart|fox|english_stem|{fox}",
            "asciihword|fox-run|english_stem|{fox-run}",
            "asciiword|quick|english_stem|{quick}",
            "hword_asciipart|run|english_stem|{run}",
        ]
    );
    assert!(
        e.execute("SELECT * FROM ts_debug('nosuchcfg', 'x')")
            .is_err()
    );
}
