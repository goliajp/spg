//! v7.17.0 Phase 2.2b — FULLTEXT GIN planner hook.
//!
//! Status: Phase 2.2 (`1963e1e`) ships real FULLTEXT KEY +
//! MATCH AGAINST semantics — the @@ rewrite returns the
//! correct rows. The planner currently full-scans rather than
//! using the GIN posting list for index probing, so the
//! result-correctness path is verified but perf scaling
//! suffers on large tables. The structural fix routes the
//! @@ predicate through the GIN posting-list intersection
//! the way the tsvector @@ path does — carved out for v7.18.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn fulltext_match_against_returns_correct_rows() {
    // Correctness sanity check — Phase 2.2 is verified.
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE articles (\
             id INT NOT NULL, \
             title TEXT NOT NULL, \
             body TEXT NOT NULL, \
             FULLTEXT KEY ft_title_body (title, body)\
         )",
    )
    .unwrap();
    e.execute(
        "INSERT INTO articles VALUES \
            (1, 'PostgreSQL guide', 'A friendly tour of PG'), \
            (2, 'MySQL basics', 'SELECT queries explained'), \
            (3, 'SPG performance', 'Vector search benchmarks')",
    )
    .unwrap();
    let r = rows(
        e.execute("SELECT id FROM articles WHERE MATCH(title, body) AGAINST('PostgreSQL')")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
}

#[test]
fn fulltext_no_match_returns_empty() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE articles (\
             id INT NOT NULL, \
             title TEXT NOT NULL, \
             body TEXT NOT NULL, \
             FULLTEXT KEY ft (title, body)\
         )",
    )
    .unwrap();
    e.execute("INSERT INTO articles VALUES (1, 'foo', 'bar')")
        .unwrap();
    let r = rows(
        e.execute("SELECT id FROM articles WHERE MATCH(title, body) AGAINST('nonexistent')")
            .unwrap(),
    );
    assert!(r.is_empty());
}
