//! v7.39 (round 245) — full-text-search sweep, 30 cases against live
//! PG18.4 (2026-07-19). to_tsvector/to_tsquery/plainto/phraseto/
//! websearch, @@ across AND/OR/NOT/phrase, ts_rank, ts_headline,
//! ts_delete/setweight/strip, ts_rewrite and the tsvector editors all
//! matched; the gaps:
//!
//!   * `fox:*` — the PREFIX query — parsed but the `:*` was thrown away
//!     (the suffix skipper discarded everything after `:`), so
//!     `foxtrot @@ fox:*` was false. The flag now rides bit 4 of the
//!     term's weight mask (no storage-format change), weight letters
//!     ride the low bits, and both print back (`'fox':*`, `'a':AB`);
//!   * `tsquery <-> tsquery` — phrase concatenation — was mistaken for
//!     the vector-distance operator that shares the spelling;
//!   * to_tsquery('english', …) kept STOPWORD terms, so `!(a & b)`
//!     demanded a lexeme no vector contains; PG prunes the stopword and
//!     collapses the tree to `!'b'`;
//!   * the stemmer missed Snowball's exceptional forms (skies→sky) and
//!     its short-word `ies`→`ie` rule (dies→die, ties→tie — Porter's
//!     unconditional `i` gave di/ti).
//!
//! Recorded residuals: ts_rewrite's operand order (PG sorts the
//! rewritten tree; QTNSort is its own round), per-position weights
//! (`'a':1A,2B` — SPG stores one weight per lexeme), and an
//! all-stopwords query (PG returns an empty tsquery; SPG has no
//! empty-tree representation).

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn prefix_queries_parse_match_and_print() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT to_tsquery('english', 'fox:*')"), "'fox':*");
    assert_eq!(
        one(&mut e, "SELECT (to_tsvector('english','foxtrot') @@ to_tsquery('english','fox:*'))::text"),
        "true"
    );
    assert_eq!(
        one(&mut e, "SELECT (to_tsvector('english','oxcart') @@ to_tsquery('english','fox:*'))::text"),
        "false"
    );
    // Weight letters survive the round trip too.
    assert_eq!(one(&mut e, "SELECT 'a:AB'::tsquery"), "'a':AB");
    // A weighted query only matches lexemes carrying that weight.
    assert_eq!(
        one(&mut e, "SELECT (setweight('a:1'::tsvector,'A') @@ 'a:A'::tsquery)::text"),
        "true"
    );
    assert_eq!(
        one(&mut e, "SELECT ('a:1'::tsvector @@ 'a:A'::tsquery)::text"),
        "false"
    );
}

#[test]
fn tsquery_phrase_operator() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 'a'::tsquery <-> 'b'::tsquery"), "'a' <-> 'b'");
    assert_eq!(
        one(&mut e, "SELECT ('a:1 b:2'::tsvector @@ ('a'::tsquery <-> 'b'::tsquery))::text"),
        "true"
    );
}

#[test]
fn to_tsquery_prunes_stopwords() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT to_tsquery('english', '!(a & b)')"), "!'b'");
    assert_eq!(one(&mut e, "SELECT to_tsquery('english', 'the & fox')"), "'fox'");
}

#[test]
fn snowball_exceptions_and_short_ies() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT to_tsvector('english','skies sky flies cries babies studies dies ties')"),
        "'babi':5 'cri':4 'die':7 'fli':3 'sky':1,2 'studi':6 'tie':8"
    );
    assert_eq!(
        one(&mut e, "SELECT (to_tsvector('english','skies') @@ to_tsquery('english','sky'))::text"),
        "true"
    );
}

#[test]
fn the_fts_core_is_unchanged() {
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "SELECT to_tsvector('english', 'The quick brown foxes jumped')",
            "'brown':3 'fox':4 'jump':5 'quick':2",
        ),
        (
            "SELECT websearch_to_tsquery('english', '\"quick fox\" -lazy or dog')",
            "'quick' <-> 'fox' & !'lazi' | 'dog'",
        ),
        (
            "SELECT (to_tsvector('english','a b c') @@ phraseto_tsquery('english','b c'))::text",
            "true",
        ),
        ("SELECT ('a:1 c:3'::tsvector @@ 'a <2> c'::tsquery)::text", "true"),
        ("SELECT ('a:1 c:3'::tsvector @@ 'a <-> c'::tsquery)::text", "false"),
        (
            "SELECT ts_headline('english', 'The quick brown fox', to_tsquery('english','fox'))",
            "The quick brown <b>fox</b>",
        ),
        ("SELECT querytree('a & !b'::tsquery)", "'a'"),
        ("SELECT setweight('a:1 b:2'::tsvector, 'A')", "'a':1A 'b':2A"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}
