//! v7.6.3 — DELETE-side FK enforcement: RESTRICT / NoAction. CASCADE
//! / SET NULL / SET DEFAULT report `Unsupported` until v7.6.4 / v7.6.5.

use spg_engine::{Engine, EngineError, QueryResult};

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        let r = eng.execute(sql).unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
        assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql:?}");
    }
    eng
}

fn rows(eng: &mut Engine, sql: &str) -> usize {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn delete_parent_with_child_reference_is_restricted() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id))",
        "INSERT INTO u VALUES (1), (2)",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    let r = eng.execute("DELETE FROM u WHERE id = 1");
    assert!(
        matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("FOREIGN KEY violation")),
        "got {r:?}"
    );
    // Parent unchanged.
    assert_eq!(rows(&mut eng, "SELECT id FROM u"), 2);
    // Child unchanged.
    assert_eq!(rows(&mut eng, "SELECT id FROM o"), 1);
}

#[test]
fn delete_unreferenced_parent_succeeds() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id))",
        "INSERT INTO u VALUES (1), (2)",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    // Row id=2 has no child references — DELETE proceeds.
    eng.execute("DELETE FROM u WHERE id = 2").unwrap();
    assert_eq!(rows(&mut eng, "SELECT id FROM u"), 1);
}

#[test]
fn delete_with_null_fk_in_child_does_not_block_parent_delete() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT REFERENCES u(id))",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (10, NULL)",
    ]);
    // Child has NULL uid → no relationship → DELETE on u proceeds.
    eng.execute("DELETE FROM u WHERE id = 1").unwrap();
    assert_eq!(rows(&mut eng, "SELECT id FROM u"), 0);
}

#[test]
fn delete_with_explicit_no_action_is_also_restricted() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL, \
         FOREIGN KEY (uid) REFERENCES u(id) ON DELETE NO ACTION)",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    let r = eng.execute("DELETE FROM u WHERE id = 1");
    assert!(matches!(r, Err(EngineError::Unsupported(_))));
}

#[test]
fn delete_with_cascade_currently_reports_not_implemented() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL, \
         FOREIGN KEY (uid) REFERENCES u(id) ON DELETE CASCADE)",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    let r = eng.execute("DELETE FROM u WHERE id = 1");
    // v7.6.3 placeholder — v7.6.4 will accept this and cascade.
    assert!(matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("CASCADE")));
}

#[test]
fn delete_with_set_null_currently_reports_not_implemented() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT, \
         FOREIGN KEY (uid) REFERENCES u(id) ON DELETE SET NULL)",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    let r = eng.execute("DELETE FROM u WHERE id = 1");
    assert!(matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("SET NULL")));
}

#[test]
fn self_referencing_delete_of_orphan_succeeds() {
    // org: id=1 is root (parent_id NULL), id=2 references 1.
    // Deleting id=2 leaves no children → succeeds.
    let mut eng = engine_with(&[
        "CREATE TABLE org (id INT NOT NULL, parent_id INT, \
         FOREIGN KEY (parent_id) REFERENCES org(id))",
        "CREATE INDEX org_pk ON org (id)",
        "INSERT INTO org VALUES (1, NULL)",
        "INSERT INTO org VALUES (2, 1)",
    ]);
    eng.execute("DELETE FROM org WHERE id = 2").unwrap();
    assert_eq!(rows(&mut eng, "SELECT id FROM org"), 1);
}

#[test]
fn self_referencing_delete_root_with_child_is_restricted() {
    let mut eng = engine_with(&[
        "CREATE TABLE org (id INT NOT NULL, parent_id INT, \
         FOREIGN KEY (parent_id) REFERENCES org(id))",
        "CREATE INDEX org_pk ON org (id)",
        "INSERT INTO org VALUES (1, NULL)",
        "INSERT INTO org VALUES (2, 1)",
    ]);
    let r = eng.execute("DELETE FROM org WHERE id = 1");
    assert!(matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("FOREIGN KEY violation")));
}

#[test]
fn delete_to_table_without_inbound_fks_is_unaffected() {
    let mut eng = engine_with(&[
        "CREATE TABLE plain (id INT NOT NULL)",
        "INSERT INTO plain VALUES (1), (2)",
    ]);
    eng.execute("DELETE FROM plain WHERE id = 1").unwrap();
    assert_eq!(rows(&mut eng, "SELECT id FROM plain"), 1);
}
