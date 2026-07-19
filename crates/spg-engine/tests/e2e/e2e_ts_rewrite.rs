//! v7.37.17 (17.6 siblings) — ts_rewrite(query, target, substitute):
//! tsquery synonym-expansion subtree rewrite.

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
        spg_storage::Value::TsQuery(_) => panic!("cast to text in the SQL"),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn single_term_synonym_expansion() {
    let mut e = Engine::new();
    // PG doc vector: ts_rewrite('a & b'::tsquery, 'a'::tsquery,
    // 'foo|bar'::tsquery) → 'b & ( foo | bar )' (rendering differs;
    // we assert the structural content via the external form).
    // v7.39 (round 245) — the simple config: the default (english) now
    // prunes 'a' as a stopword before ts_rewrite ever sees it, exactly as
    // PG does, which would make this test assert nothing.
    let got = first_text(
        &mut e,
        "SELECT ts_rewrite(to_tsquery('simple', 'a & b'), to_tsquery('simple', 'a'), \
         to_tsquery('simple', 'foo | bar'))::text",
    );
    assert!(
        got.contains("foo") && got.contains("bar") && got.contains('b'),
        "unexpected rewrite: {got}"
    );
    assert!(!got.contains("'a'"), "target should be replaced: {got}");
}

#[test]
fn no_match_leaves_query_unchanged() {
    let mut e = Engine::new();
    let got = first_text(
        &mut e,
        "SELECT ts_rewrite(to_tsquery('x & y'), to_tsquery('z'), \
         to_tsquery('w'))::text",
    );
    assert!(
        got.contains('x') && got.contains('y') && !got.contains('w'),
        "unexpected rewrite: {got}"
    );
}

#[test]
fn whole_query_replacement() {
    let mut e = Engine::new();
    // Target equals the whole query → substitute replaces it all.
    let got = first_text(
        &mut e,
        "SELECT ts_rewrite(to_tsquery('a'), to_tsquery('a'), \
         to_tsquery('b & c'))::text",
    );
    assert!(
        got.contains('b') && got.contains('c') && !got.contains("'a'"),
        "unexpected rewrite: {got}"
    );
}
