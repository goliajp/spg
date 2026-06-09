//! v7.17.0 Phase 3.P0-44 — MATCH AGAINST routes through the GIN
//! posting-list seek instead of full-scanning.
//!
//! Phase 2.2 (`1963e1e`) made `MATCH(col) AGAINST ('term')` desugar
//! into `to_tsvector('simple', col) @@ plainto_tsquery('simple',
//! 'term')`, but the planner's GIN seek arm only recognised a raw
//! `Expr::Column` on the col side and only matched `IndexKind::Gin`.
//! `FULLTEXT KEY` indexes (`IndexKind::GinFulltext`) and the
//! `to_tsvector(col)` wrapper were both invisible, so MATCH AGAINST
//! quietly full-scanned even when the FULLTEXT index existed.
//!
//! These tests pin the post-fix surface by checking correctness on
//! several shapes the planner now accelerates:
//!   * single-column MATCH AGAINST against a FULLTEXT KEY
//!   * multi-column MATCH AGAINST (parser-generated OR-fold)
//!   * AND combination with a non-fulltext predicate (the planner
//!     still seeks the FULLTEXT side)
//!   * NULL / no-match / unindexed-column fall-through stays correct

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

fn seed_articles_with_fulltext(e: &mut Engine) {
    e.execute(
        "CREATE TABLE articles (\
             id INT NOT NULL, \
             title TEXT NOT NULL, \
             body TEXT NOT NULL, \
             FULLTEXT KEY ft_title (title), \
             FULLTEXT KEY ft_body (body)\
         )",
    )
    .unwrap();
    // Note: plainto_tsquery uses AND across terms — so multi-word
    // AGAINST literals must use a SINGLE-term term to test the
    // OR-fold across columns. Row 1 has 'postgresql' in title and
    // row 6 has 'postgresql' in body, so MATCH(title, body)
    // AGAINST ('PostgreSQL') exercises the union path.
    e.execute(
        "INSERT INTO articles VALUES \
            (1, 'PostgreSQL guide', 'A friendly tour of PG'), \
            (2, 'MySQL basics', 'SELECT queries explained'), \
            (3, 'SPG performance', 'Vector search benchmarks'), \
            (4, 'Quick fox', 'The quick brown fox jumps over the lazy dog'), \
            (5, 'Rust async', 'tokio runtime internals'), \
            (6, 'Untitled', 'A digest of PostgreSQL internals')",
    )
    .unwrap();
}

#[test]
fn single_col_match_against_returns_correct_rows() {
    let mut e = Engine::new();
    seed_articles_with_fulltext(&mut e);
    let r = rows(
        e.execute("SELECT id FROM articles WHERE MATCH(title) AGAINST ('PostgreSQL')")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
}

#[test]
fn multi_col_match_against_unions_both_indexes() {
    // 'postgresql' lemma lives in row 1's title and in row 6's body,
    // so the parser's `OR`-fold across `to_tsvector(title) @@ q` and
    // `to_tsvector(body) @@ q` should seek both FULLTEXT KEYs and
    // union the candidate sets. Single-term AGAINST literal so the
    // `plainto_tsquery` AND-across-words rule doesn't bite.
    let mut e = Engine::new();
    seed_articles_with_fulltext(&mut e);
    let mut r = rows(
        e.execute(
            "SELECT id FROM articles WHERE MATCH(title, body) AGAINST ('PostgreSQL')",
        )
        .unwrap(),
    );
    r.sort_by_key(|row| match row[0] {
        Value::Int(n) => n,
        _ => panic!(),
    });
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(6));
}

#[test]
fn match_against_combined_with_simple_predicate() {
    // AND with an `id` filter — Phase 2.2 AND-recursion already
    // walks the conjuncts, so the FULLTEXT side still seeks.
    let mut e = Engine::new();
    seed_articles_with_fulltext(&mut e);
    let r = rows(
        e.execute(
            "SELECT id FROM articles WHERE MATCH(body) AGAINST ('quick fox') AND id = 4",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(4));
}

#[test]
fn match_against_with_no_matching_term_returns_empty() {
    let mut e = Engine::new();
    seed_articles_with_fulltext(&mut e);
    let r = rows(
        e.execute("SELECT id FROM articles WHERE MATCH(title, body) AGAINST ('nonexistent')")
            .unwrap(),
    );
    assert!(r.is_empty());
}

#[test]
fn match_against_when_no_fulltext_index_falls_through() {
    // No FULLTEXT KEY — the planner can't seek. Phase 2.2's
    // correctness path (full-scan @@ rewrite) must still return
    // the right rows.
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE plain (id INT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL)",
    )
    .unwrap();
    e.execute(
        "INSERT INTO plain VALUES \
            (1, 'PostgreSQL guide', 'A friendly tour of PG'), \
            (2, 'MySQL basics', 'SELECT queries explained')",
    )
    .unwrap();
    let r = rows(
        e.execute("SELECT id FROM plain WHERE MATCH(title) AGAINST ('PostgreSQL')")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
}

#[test]
fn direct_tsvector_at_at_path_still_works_regression() {
    // The v7.12 `col @@ tsquery` GIN path is unchanged by P0-44;
    // peeling the to_tsvector wrapper must NOT break the direct
    // Column-on-col-side case.
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE docs (\
             id INT NOT NULL, \
             vec TSVECTOR NOT NULL\
         )",
    )
    .unwrap();
    e.execute("CREATE INDEX idx_vec ON docs USING gin (vec)")
        .unwrap();
    e.execute(
        "INSERT INTO docs VALUES \
            (1, to_tsvector('simple', 'PostgreSQL guide')), \
            (2, to_tsvector('simple', 'MySQL basics'))",
    )
    .unwrap();
    let r = rows(
        e.execute("SELECT id FROM docs WHERE vec @@ plainto_tsquery('simple', 'PostgreSQL')")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
}

#[test]
fn match_against_or_with_unrelated_predicate_falls_through() {
    // OR with a side that can't seek (the non-MATCH AGAINST conjunct)
    // returns None from the OR walker so the engine full-scans.
    // Correctness must still hold.
    let mut e = Engine::new();
    seed_articles_with_fulltext(&mut e);
    let mut r = rows(
        e.execute(
            "SELECT id FROM articles WHERE MATCH(title) AGAINST ('PostgreSQL') OR id = 3",
        )
        .unwrap(),
    );
    r.sort_by_key(|row| match row[0] {
        Value::Int(n) => n,
        _ => panic!(),
    });
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(3));
}
