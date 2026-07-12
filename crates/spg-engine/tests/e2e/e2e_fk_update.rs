//! v7.6.6 — UPDATE-side FK enforcement (both directions).

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

fn select(eng: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn updating_child_fk_to_existing_parent_succeeds() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id))",
        "INSERT INTO u VALUES (1), (2)",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    eng.execute("UPDATE o SET uid = 2 WHERE id = 10").unwrap();
    let rows = select(&mut eng, "SELECT uid FROM o");
    assert_eq!(rows[0], vec![Value::Int(2)]);
}

#[test]
fn updating_child_fk_to_missing_parent_is_rejected() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id))",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    let r = eng.execute("UPDATE o SET uid = 99 WHERE id = 10");
    assert!(
        matches!(r, Err(EngineError::Unsupported(ref s)) if s.to_lowercase().contains("foreign key"))
    );
    // No change committed.
    assert_eq!(
        select(&mut eng, "SELECT uid FROM o")[0],
        vec![Value::Int(1)]
    );
}

#[test]
fn updating_parent_pk_with_default_restrict_is_rejected_when_child_references() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id))",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    let r = eng.execute("UPDATE u SET id = 2 WHERE id = 1");
    assert!(
        matches!(r, Err(EngineError::Unsupported(ref s)) if s.to_lowercase().contains("foreign key"))
    );
}

#[test]
fn updating_parent_pk_with_on_update_cascade_rewrites_child_fk() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL, \
         FOREIGN KEY (uid) REFERENCES u(id) ON UPDATE CASCADE)",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (10, 1), (11, 1)",
    ]);
    eng.execute("UPDATE u SET id = 99 WHERE id = 1").unwrap();
    let rows = select(&mut eng, "SELECT uid FROM o ORDER BY id");
    // Both child rows should now reference 99.
    assert_eq!(rows[0], vec![Value::Int(99)]);
    assert_eq!(rows[1], vec![Value::Int(99)]);
}

#[test]
fn updating_parent_pk_with_set_null_blanks_child_fk() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT, \
         FOREIGN KEY (uid) REFERENCES u(id) ON UPDATE SET NULL)",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    eng.execute("UPDATE u SET id = 2 WHERE id = 1").unwrap();
    // u parent is now id=2; o(10).uid was set to NULL.
    let rows = select(&mut eng, "SELECT uid FROM o");
    assert_eq!(rows[0], vec![Value::Null]);
}

#[test]
fn updating_parent_unrelated_column_does_not_trigger_fk_action() {
    // u has (id, name). FK references u(id). Updating u.name
    // should not touch the child table.
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL, name TEXT)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id))",
        "INSERT INTO u VALUES (1, 'alice')",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    eng.execute("UPDATE u SET name = 'bob' WHERE id = 1")
        .unwrap();
    // o unchanged.
    let rows = select(&mut eng, "SELECT uid FROM o");
    assert_eq!(rows[0], vec![Value::Int(1)]);
}

#[test]
fn updating_child_fk_to_null_does_not_require_parent() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT, \
         FOREIGN KEY (uid) REFERENCES u(id))",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    eng.execute("UPDATE o SET uid = NULL WHERE id = 10")
        .unwrap();
    let rows = select(&mut eng, "SELECT uid FROM o");
    assert_eq!(rows[0], vec![Value::Null]);
}
