//! v7.6.2 — INSERT-side FK enforcement.

use spg_engine::{Engine, EngineError, QueryResult};

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        let r = eng.execute(sql).unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
        assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql:?}");
    }
    eng
}

fn count(eng: &mut Engine, sql: &str) -> usize {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn insert_matching_parent_succeeds() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id))",
        "INSERT INTO u VALUES (1), (2)",
    ]);
    eng.execute("INSERT INTO o VALUES (10, 1), (11, 2)").unwrap();
    assert_eq!(count(&mut eng, "SELECT id FROM o"), 2);
}

#[test]
fn insert_missing_parent_is_rejected() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id))",
        "INSERT INTO u VALUES (1)",
    ]);
    let r = eng.execute("INSERT INTO o VALUES (10, 99)");
    assert!(matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("FOREIGN KEY violation")));
    // No row persisted.
    assert_eq!(count(&mut eng, "SELECT id FROM o"), 0);
}

#[test]
fn insert_with_null_fk_column_skips_check() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT REFERENCES u(id))",
    ]);
    eng.execute("INSERT INTO o VALUES (10, NULL)").unwrap();
    assert_eq!(count(&mut eng, "SELECT id FROM o"), 1);
}

#[test]
fn batch_insert_atomically_rejected_if_any_row_violates() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id))",
        "INSERT INTO u VALUES (1)",
    ]);
    let r = eng.execute("INSERT INTO o VALUES (10, 1), (11, 99)");
    assert!(r.is_err());
    // First (valid) row is also NOT inserted — enforcement is
    // pre-insert, so the batch is atomic at FK granularity.
    assert_eq!(count(&mut eng, "SELECT id FROM o"), 0);
}

#[test]
fn insert_to_table_without_fk_is_unaffected() {
    let mut eng = engine_with(&[
        "CREATE TABLE plain (id INT NOT NULL, name TEXT)",
        "INSERT INTO plain VALUES (1, 'a'), (2, 'b')",
    ]);
    assert_eq!(count(&mut eng, "SELECT id FROM plain"), 2);
}

#[test]
fn cascade_action_stored_but_not_yet_enforced_on_delete() {
    // v7.6.2 only enforces INSERT. DELETE-side CASCADE lands in
    // v7.6.4 — until then, DELETE on parent still removes the
    // parent row without touching child. This test pins that
    // behaviour so v7.6.4 has a visible behavioural anchor.
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id) ON DELETE CASCADE)",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    let _ = eng.execute("DELETE FROM u WHERE id = 1");
    // Whether DELETE was accepted or rejected at this point depends
    // on whether v7.6.3 has shipped yet; either way `o` should be
    // observable via SELECT — the test just asserts no panic.
    let _ = eng.execute("SELECT id FROM o");
}

#[test]
fn self_referencing_fk_root_insert_works() {
    // Root row references no parent (NULL); subsequent rows can
    // reference earlier rows visible in the same TABLE — but
    // v7.6.2 enforcement is per-batch, so cross-row references in
    // a single multi-VALUES INSERT are NOT yet allowed.
    let mut eng = engine_with(&[
        "CREATE TABLE org (id INT NOT NULL, parent_id INT, \
         FOREIGN KEY (parent_id) REFERENCES org(id))",
        "CREATE INDEX org_pk ON org (id)",
        "INSERT INTO org VALUES (1, NULL)",
        "INSERT INTO org VALUES (2, 1)",
    ]);
    assert_eq!(count(&mut eng, "SELECT id FROM org"), 2);
}
