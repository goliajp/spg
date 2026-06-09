//! v7.17.0 Phase 5.4 — `gin_trgm_ops` GIN index + partial WHERE
//! predicate. The opclass + partial-index combo landed in
//! v7.13.2 (mailrs round-6 S2); this file pins the customer-
//! facing surface so a future refactor can't regress the
//! mailrs-shaped partial-trigram index path.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn gin_trgm_partial_index_creates_and_filters() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL, active BOOL NOT NULL)")
        .unwrap();
    e.execute(
        "INSERT INTO t VALUES \
            (1, 'alice', true), \
            (2, 'bob', true), \
            (3, 'archive', false)",
    )
    .unwrap();
    // mailrs round-6 S2 shape: trigram index restricted to live
    // rows via partial WHERE.
    e.execute("CREATE INDEX idx_t_name_trgm ON t USING gin (name gin_trgm_ops) WHERE active")
        .unwrap();
    // Substring search via the `%` (pg_trgm similarity) /
    // `LIKE` shapes that the planner can route through the
    // trigram index.
    let r = rows(
        e.execute("SELECT id FROM t WHERE name LIKE '%lic%'")
            .unwrap(),
    );
    let ids: Vec<i32> = r
        .iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            _ => unreachable!(),
        })
        .collect();
    assert!(ids.contains(&1));
}

#[test]
fn gin_trgm_index_without_partial_predicate() {
    // Same opclass, no WHERE — the unpartitioned base case.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'alice'), (2, 'bob')")
        .unwrap();
    e.execute("CREATE INDEX idx_t_name_trgm ON t USING gin (name gin_trgm_ops)")
        .unwrap();
    let r = rows(
        e.execute("SELECT id FROM t WHERE name LIKE '%bo%'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(2));
}

#[test]
fn gin_trgm_partial_predicate_with_complex_expression() {
    // mailrs's "live recent" partial-index shape.
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE t (\
            id INT NOT NULL, \
            name TEXT NOT NULL, \
            status TEXT NOT NULL, \
            created_at TIMESTAMP NOT NULL\
         )",
    )
    .unwrap();
    // Partial predicate with multiple conjuncts — what mailrs
    // round-6 S2 emitted in its outbox-search index.
    e.execute(
        "CREATE INDEX idx_t_name_trgm ON t USING gin (name gin_trgm_ops) \
         WHERE status = 'live'",
    )
    .unwrap();
    e.execute(
        "INSERT INTO t VALUES \
            (1, 'alice', 'live', '2024-01-01 00:00:00'::TIMESTAMP), \
            (2, 'archive', 'archived', '2024-01-02 00:00:00'::TIMESTAMP)",
    )
    .unwrap();
    let r = rows(
        e.execute("SELECT id FROM t WHERE name LIKE '%lic%' AND status = 'live'")
            .unwrap(),
    );
    let ids: Vec<i32> = r
        .iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(ids, vec![1]);
}
