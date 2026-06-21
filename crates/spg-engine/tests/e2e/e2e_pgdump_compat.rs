//! v7.9.26b + v7.9.27b — pg_dump compatibility: GIN/GiST/SPGiST
//! index methods accepted as BTree, IS [NOT] DISTINCT FROM
//! evaluates with NULL-safe semantics.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        let r = eng
            .execute(sql)
            .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"));
        assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql:?}");
    }
    eng
}

fn rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn gin_index_accepted_as_btree_fallback() {
    let mut eng = engine_with(&[
        "CREATE TABLE docs (id INT NOT NULL, payload JSONB)",
        "CREATE INDEX docs_payload ON docs USING gin (payload)",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    assert!(
        cat.get("docs")
            .unwrap()
            .indices()
            .iter()
            .any(|i| i.name == "docs_payload")
    );
}

#[test]
fn gist_and_spgist_and_hash_accepted_too() {
    let _eng = engine_with(&[
        "CREATE TABLE t (a INT NOT NULL, g TEXT, s TEXT, h TEXT)",
        "CREATE INDEX t_g ON t USING gist (g)",
        "CREATE INDEX t_s ON t USING spgist (s)",
        "CREATE INDEX t_h ON t USING hash (h)",
    ]);
}

#[test]
fn is_not_distinct_from_null_null_is_true() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (a INT, b INT)",
        "INSERT INTO t VALUES (NULL, NULL), (1, 1), (1, NULL)",
    ]);
    let r = rows(&mut eng, "SELECT a IS NOT DISTINCT FROM b FROM t");
    assert_eq!(r[0][0], Value::Bool(true)); // (NULL, NULL)
    assert_eq!(r[1][0], Value::Bool(true)); // (1, 1)
    assert_eq!(r[2][0], Value::Bool(false)); // (1, NULL)
}

#[test]
fn is_distinct_from_inverts() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (a INT, b INT)",
        "INSERT INTO t VALUES (NULL, NULL), (1, 1), (1, NULL)",
    ]);
    let r = rows(&mut eng, "SELECT a IS DISTINCT FROM b FROM t");
    assert_eq!(r[0][0], Value::Bool(false)); // (NULL, NULL)
    assert_eq!(r[1][0], Value::Bool(false)); // (1, 1)
    assert_eq!(r[2][0], Value::Bool(true)); // (1, NULL)
}

#[test]
fn is_not_distinct_from_in_where_clause() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL, parent INT)",
        "INSERT INTO u VALUES (1, NULL), (2, 1), (3, NULL)",
    ]);
    // NULL = NULL via IS NOT DISTINCT FROM.
    let r = rows(
        &mut eng,
        "SELECT id FROM u WHERE parent IS NOT DISTINCT FROM NULL ORDER BY id",
    );
    assert_eq!(r.len(), 2);
}

#[test]
fn is_null_still_works_after_widening() {
    let mut eng = engine_with(&["CREATE TABLE t (a INT)", "INSERT INTO t VALUES (NULL), (1)"]);
    let r = rows(&mut eng, "SELECT a IS NULL FROM t");
    assert_eq!(r[0][0], Value::Bool(true));
    assert_eq!(r[1][0], Value::Bool(false));
}
