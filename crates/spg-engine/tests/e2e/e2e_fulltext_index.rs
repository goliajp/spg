//! v7.17.0 Phase 2.2 — MySQL `FULLTEXT KEY` parsing + real
//! tsvector-GIN install + MATCH AGAINST query surface.
//!
//! Pre-v7.17 SPG silently dropped FULLTEXT declarations at the
//! parser, so mysqldump-imported tables ended up with no inverted
//! index at all and `MATCH AGAINST` queries failed to parse. This
//! test crate locks the three guarantees the customer-facing fix
//! gives them:
//!
//!   1. CREATE TABLE with `FULLTEXT KEY name (col)` succeeds and
//!      the table reports a `GinFulltext` index on `col`.
//!   2. INSERTs populate the posting list — the index's GIN
//!      lookup against a tokenised word returns the inserted
//!      row's locator.
//!   3. `WHERE MATCH(col) AGAINST ('term')` parses and returns
//!      the rows whose `col` contains every term lexeme. Mode
//!      modifiers (`IN BOOLEAN MODE`, `IN NATURAL LANGUAGE MODE
//!      WITH QUERY EXPANSION`) are accepted-and-ignored.

use spg_engine::Engine;
use spg_engine::QueryResult;
use spg_storage::IndexKind;

fn rows(r: QueryResult) -> Vec<spg_storage::Row> {
    match r {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows"),
    }
}

#[test]
fn fulltext_key_creates_gin_fulltext_index() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE posts (\
             id INT NOT NULL,\
             body TEXT,\
             FULLTEXT KEY body_ft (body)\
         )",
    )
    .unwrap();
    let cat = e.catalog();
    let t = cat.get("posts").expect("posts table installed");
    let body_pos = t.schema().column_position("body").unwrap();
    let ft = t
        .indices()
        .iter()
        .find(|i| matches!(i.kind, IndexKind::GinFulltext(_)))
        .expect("FULLTEXT KEY should install a GinFulltext index");
    assert_eq!(ft.column_position, body_pos);
    assert_eq!(ft.name, "body_ft");
}

#[test]
fn insert_populates_fulltext_posting_list() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE posts (\
             id INT NOT NULL,\
             body TEXT,\
             FULLTEXT KEY body_ft (body)\
         )",
    )
    .unwrap();
    e.execute("INSERT INTO posts VALUES (1, 'Quick brown fox')")
        .unwrap();
    e.execute("INSERT INTO posts VALUES (2, 'Lazy DOG sleeping')")
        .unwrap();
    let cat = e.catalog();
    let t = cat.get("posts").unwrap();
    let ft = t
        .indices()
        .iter()
        .find(|i| matches!(i.kind, IndexKind::GinFulltext(_)))
        .unwrap();
    // simple_lex lower-cases, so the keys live under the lowered
    // forms regardless of source casing.
    assert!(
        !ft.gin_lookup_word("brown").is_empty(),
        "row 1 should be indexed under 'brown'"
    );
    assert!(
        !ft.gin_lookup_word("dog").is_empty(),
        "row 2's uppercase 'DOG' should land under 'dog'"
    );
    // A non-indexed word returns empty.
    assert!(
        ft.gin_lookup_word("nonexistent").is_empty(),
        "word that never appeared should miss"
    );
}

#[test]
fn match_against_finds_matching_rows() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE posts (\
             id INT NOT NULL,\
             body TEXT,\
             FULLTEXT KEY body_ft (body)\
         )",
    )
    .unwrap();
    e.execute("INSERT INTO posts VALUES (1, 'Quick brown fox')")
        .unwrap();
    e.execute("INSERT INTO posts VALUES (2, 'Lazy dog sleeping')")
        .unwrap();
    e.execute("INSERT INTO posts VALUES (3, 'Foxes and dogs play')")
        .unwrap();
    let out = rows(
        e.execute("SELECT id FROM posts WHERE MATCH(body) AGAINST ('brown fox') ORDER BY id")
            .unwrap(),
    );
    assert_eq!(out.len(), 1, "only row 1 contains both 'brown' and 'fox'");
    assert_eq!(out[0].values[0], spg_storage::Value::Int(1));
}

#[test]
fn match_against_boolean_mode_accepted() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE posts (\
             id INT NOT NULL,\
             body TEXT,\
             FULLTEXT KEY body_ft (body)\
         )",
    )
    .unwrap();
    e.execute("INSERT INTO posts VALUES (1, 'database tuning guide')")
        .unwrap();
    e.execute("INSERT INTO posts VALUES (2, 'recipe book')")
        .unwrap();
    let out = rows(
        e.execute(
            "SELECT id FROM posts \
                 WHERE MATCH(body) AGAINST ('database' IN BOOLEAN MODE) \
                 ORDER BY id",
        )
        .unwrap(),
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].values[0], spg_storage::Value::Int(1));
}

#[test]
fn match_against_natural_language_mode_with_query_expansion_accepted() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE posts (\
             id INT NOT NULL,\
             body TEXT,\
             FULLTEXT KEY body_ft (body)\
         )",
    )
    .unwrap();
    e.execute("INSERT INTO posts VALUES (1, 'spring summer autumn')")
        .unwrap();
    // The mode modifier is accept-and-ignore — the rewrite still
    // hits the bare-term lexeme match.
    let out = rows(
        e.execute(
            "SELECT id FROM posts \
                 WHERE MATCH(body) AGAINST ('autumn' IN NATURAL LANGUAGE MODE WITH QUERY EXPANSION) \
                 ORDER BY id",
        )
        .unwrap(),
    );
    assert_eq!(out.len(), 1);
}

#[test]
fn match_against_multi_column_or_folds() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE posts (\
             id INT NOT NULL,\
             title TEXT,\
             body TEXT,\
             FULLTEXT KEY t_b_ft (title, body)\
         )",
    )
    .unwrap();
    e.execute("INSERT INTO posts VALUES (1, 'Mountains', 'in summer')")
        .unwrap();
    e.execute("INSERT INTO posts VALUES (2, 'Rivers', 'mountains nearby')")
        .unwrap();
    let out = rows(
        e.execute(
            "SELECT id FROM posts WHERE MATCH(title, body) AGAINST ('mountains') ORDER BY id",
        )
        .unwrap(),
    );
    let ids: Vec<i32> = out
        .iter()
        .map(|r| match r.values[0] {
            spg_storage::Value::Int(n) => n,
            _ => panic!("id is INT"),
        })
        .collect();
    assert_eq!(ids, alloc::vec![1_i32, 2]);
}

#[test]
fn fulltext_index_survives_catalog_roundtrip() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE posts (\
             id INT NOT NULL,\
             body TEXT,\
             FULLTEXT KEY body_ft (body)\
         )",
    )
    .unwrap();
    e.execute("INSERT INTO posts VALUES (1, 'Quick brown fox')")
        .unwrap();
    let bytes = e.catalog().serialize();
    let cat = spg_storage::Catalog::deserialize(&bytes).expect("catalog round-trips");
    let t = cat.get("posts").expect("posts persisted");
    let ft = t
        .indices()
        .iter()
        .find(|i| matches!(i.kind, IndexKind::GinFulltext(_)))
        .expect("FULLTEXT index restored");
    assert_eq!(ft.name, "body_ft");
    // Posting list survives — same lookup as the in-memory test.
    assert!(!ft.gin_lookup_word("brown").is_empty());
}

extern crate alloc;
