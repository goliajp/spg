//! v7.39 (read01 utils/adt, round 43) — FTS tokenizer / query
//! introspection gaps closed against PG18: ts_lexize stemming +
//! stopword drop, phraseto_tsquery stopword position gaps,
//! tsvector_to_array / array_to_tsvector accepting & returning real
//! tsvector values, and json_to_tsvector producing a positioned
//! stemmed tsvector. All byte-locked with the config spelled
//! explicitly ('english' / 'simple').

use spg_engine::{Engine, QueryResult};

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn ts_lexize_stems_and_drops_stopwords() {
    let mut e = Engine::new();
    // english_stem: stem survivors, empty array for stopwords/empty.
    assert_eq!(
        r1(&mut e, "SELECT ts_lexize('english_stem', 'jumping')"),
        "{jump}"
    );
    assert_eq!(
        r1(&mut e, "SELECT ts_lexize('english_stem', 'CATS')"),
        "{cat}"
    );
    assert_eq!(r1(&mut e, "SELECT ts_lexize('english_stem', 'the')"), "{}");
    assert_eq!(r1(&mut e, "SELECT ts_lexize('english_stem', '')"), "{}");
    // simple: lowercase only, no stopword drop.
    assert_eq!(r1(&mut e, "SELECT ts_lexize('simple', 'The')"), "{the}");
}

#[test]
fn phraseto_tsquery_honours_stopword_gaps() {
    let mut e = Engine::new();
    // 'and' is dropped but advances position → <2>.
    assert_eq!(
        r1(
            &mut e,
            "SELECT phraseto_tsquery('english', 'cats and dogs')"
        ),
        "'cat' <2> 'dog'"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT phraseto_tsquery('english', 'the quick and the brown fox')"
        ),
        "'quick' <3> 'brown' <-> 'fox'"
    );
    // simple keeps every token adjacent.
    assert_eq!(
        r1(&mut e, "SELECT phraseto_tsquery('simple', 'cats and dogs')"),
        "'cats' <-> 'and' <-> 'dogs'"
    );
}

#[test]
fn tsvector_array_round_trip_values() {
    let mut e = Engine::new();
    // tsvector_to_array accepts a real tsvector value (not only text).
    assert_eq!(
        r1(
            &mut e,
            "SELECT tsvector_to_array(to_tsvector('english','the quick brown fox'))"
        ),
        "{brown,fox,quick}"
    );
    // array_to_tsvector returns a real tsvector rendering with quotes.
    assert_eq!(
        r1(
            &mut e,
            "SELECT array_to_tsvector(ARRAY['quick','brown','quick'])"
        ),
        "'brown' 'quick'"
    );
}

#[test]
fn json_to_tsvector_positions_and_stems() {
    let mut e = Engine::new();
    // Per-value concat with a one-position gap: quick:1 fox:2, then 42:4.
    assert_eq!(
        r1(
            &mut e,
            r#"SELECT json_to_tsvector('english', '{"a":"quick fox", "b": 42}'::json, '["string","numeric"]')"#
        ),
        "'42':4 'fox':2 'quick':1"
    );
    // string-only: 'the' dropped, quick:2 brown:3.
    assert_eq!(
        r1(
            &mut e,
            r#"SELECT json_to_tsvector('english', '{"a":"The Quick Brown"}'::json, '["string"]')"#
        ),
        "'brown':3 'quick':2"
    );
}
