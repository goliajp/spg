//! v7.39 (read01 utils/adt, round 44) — the FTS pipeline default config
//! is now 'english' (PG's initdb default), not 'simple'. Bare
//! to_tsvector / to_tsquery / plainto / phraseto stem and drop stopwords
//! out of the box, and `default_text_search_config` reports
//! pg_catalog.english. Byte-locked vs PG18.

use spg_engine::{Engine, QueryResult};

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn bare_to_tsvector_defaults_to_english() {
    let mut e = Engine::new();
    // 'the' dropped (stopword), 'foxes'→'fox' (stem).
    assert_eq!(
        r1(&mut e, "SELECT to_tsvector('the quick brown foxes')"),
        "'brown':3 'fox':4 'quick':2"
    );
}

#[test]
fn bare_to_tsquery_stems_and_matches() {
    let mut e = Engine::new();
    // running→run under the english default, so the match holds.
    assert_eq!(
        r1(&mut e, "SELECT to_tsvector('running') @@ to_tsquery('run')"),
        "true"
    );
    assert_eq!(
        r1(&mut e, "SELECT plainto_tsquery('the quick foxes')"),
        "'quick' & 'fox'"
    );
}

#[test]
fn default_text_search_config_reports_english() {
    let mut e = Engine::new();
    // The GUC boot default matches PG's initdb value.
    match e.execute("SHOW default_text_search_config").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(
                spg_engine::eval::value_to_text(&rows[0].values[0]),
                "pg_catalog.english"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn explicit_simple_still_available() {
    let mut e = Engine::new();
    // Opting into 'simple' keeps the raw, unstemmed behaviour.
    assert_eq!(
        r1(&mut e, "SELECT to_tsvector('simple', 'the running foxes')"),
        "'foxes':3 'running':2 'the':1"
    );
}
