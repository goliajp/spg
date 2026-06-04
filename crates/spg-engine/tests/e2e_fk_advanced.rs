//! v7.6.7 — self-ref bulk-insert + composite FK + DEFERRABLE rejection.

use spg_engine::{Engine, EngineError, QueryResult};
use spg_storage::Value;

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        let r = eng
            .execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
        assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql:?}");
    }
    eng
}

fn count(eng: &mut Engine, sql: &str) -> usize {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => panic!("rows"),
    }
}

#[test]
fn self_ref_batch_insert_chain_within_one_statement() {
    // v7.6.7 widens v7.6.2 — multi-VALUES INSERT can reference
    // earlier rows in the same batch.
    let mut eng = engine_with(&[
        "CREATE TABLE org (id INT NOT NULL, parent_id INT, \
         FOREIGN KEY (parent_id) REFERENCES org(id))",
        "CREATE INDEX org_pk ON org (id)",
    ]);
    eng.execute("INSERT INTO org VALUES (1, NULL), (2, 1), (3, 2), (4, 3)")
        .unwrap();
    assert_eq!(count(&mut eng, "SELECT id FROM org"), 4);
}

#[test]
fn self_ref_forward_reference_in_same_batch_is_rejected() {
    // Row referencing a LATER row in the same batch is not allowed
    // — only backward references work (parent must appear first).
    let mut eng = engine_with(&[
        "CREATE TABLE org (id INT NOT NULL, parent_id INT, \
         FOREIGN KEY (parent_id) REFERENCES org(id))",
        "CREATE INDEX org_pk ON org (id)",
    ]);
    // (2, 3) references row 3 which hasn't appeared yet.
    let r = eng.execute("INSERT INTO org VALUES (2, 3), (3, NULL)");
    assert!(
        matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("FOREIGN KEY violation"))
    );
}

#[test]
fn composite_fk_insert_check_against_committed_parent() {
    // Parent indexed on first column (a). Composite FK references
    // (a, b). v7.6.2 falls back to a parent-row scan for composites
    // — verified by this test.
    let mut eng = engine_with(&[
        "CREATE TABLE p (a INT NOT NULL, b INT NOT NULL)",
        "CREATE INDEX p_a ON p (a)",
        "CREATE TABLE c (a INT NOT NULL, b INT NOT NULL, \
         FOREIGN KEY (a, b) REFERENCES p(a, b))",
        "INSERT INTO p VALUES (1, 10), (2, 20)",
    ]);
    eng.execute("INSERT INTO c VALUES (1, 10)").unwrap();
    let r = eng.execute("INSERT INTO c VALUES (1, 99)");
    assert!(matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("composite key")));
}

#[test]
fn composite_fk_batch_self_ref_supported() {
    let mut eng = engine_with(&[
        "CREATE TABLE tree (a INT NOT NULL, b INT NOT NULL, pa INT, pb INT, \
         FOREIGN KEY (pa, pb) REFERENCES tree(a, b))",
        "CREATE INDEX tree_a ON tree (a)",
    ]);
    // Root + one child referencing the root by composite key.
    eng.execute("INSERT INTO tree VALUES (1, 10, NULL, NULL), (2, 20, 1, 10)")
        .unwrap();
    assert_eq!(count(&mut eng, "SELECT a FROM tree"), 2);
}

#[test]
fn deferrable_clause_is_rejected_at_parse_time() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE u (id INT NOT NULL)").unwrap();
    eng.execute("CREATE INDEX u_pk ON u (id)").unwrap();
    let r = eng.execute(
        "CREATE TABLE o (uid INT NOT NULL REFERENCES u(id) DEFERRABLE INITIALLY DEFERRED)",
    );
    assert!(
        matches!(r, Err(EngineError::Parse(_))),
        "DEFERRABLE must surface as a parse error, got {r:?}"
    );
}

#[test]
fn not_deferrable_clause_is_accepted_silently() {
    // PG dumps often include `NOT DEFERRABLE INITIALLY IMMEDIATE`
    // even though it's the default — accept without complaint.
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (uid INT NOT NULL REFERENCES u(id) NOT DEFERRABLE INITIALLY IMMEDIATE)",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (1)",
    ]);
    let _ = eng; // build asserted by `engine_with` succeeding.
}

#[test]
fn self_ref_cascade_subtree_after_bulk_insert() {
    // Combines v7.6.7 (bulk-insert self-ref) with v7.6.4 (CASCADE).
    let mut eng = engine_with(&[
        "CREATE TABLE node (id INT NOT NULL, parent_id INT, \
         FOREIGN KEY (parent_id) REFERENCES node(id) ON DELETE CASCADE)",
        "CREATE INDEX node_pk ON node (id)",
        "INSERT INTO node VALUES (1, NULL), (2, 1), (3, 1), (4, 2)",
    ]);
    eng.execute("DELETE FROM node WHERE id = 1").unwrap();
    // Whole subtree gone.
    let rows = match eng.execute("SELECT id FROM node").unwrap() {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!("rows"),
    };
    let _: Vec<&Value> = rows.iter().map(|r| &r.values[0]).collect();
    assert!(rows.is_empty());
}
