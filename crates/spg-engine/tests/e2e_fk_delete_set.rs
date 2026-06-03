//! v7.6.5 — DELETE ON SET NULL / SET DEFAULT.

use spg_engine::{Engine, EngineError, QueryResult};
use spg_storage::Value;

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        let r = eng.execute(sql).unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
        assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql:?}");
    }
    eng
}

fn select_rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Value>> {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn set_null_writes_null_into_child_fk_column() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT, \
         FOREIGN KEY (uid) REFERENCES u(id) ON DELETE SET NULL)",
        "INSERT INTO u VALUES (1), (2)",
        "INSERT INTO o VALUES (10, 1), (11, 2)",
    ]);
    eng.execute("DELETE FROM u WHERE id = 1").unwrap();
    let mut rows = select_rows(&mut eng, "SELECT id, uid FROM o ORDER BY id");
    rows.sort_by_key(|r| match &r[0] {
        Value::Int(n) => *n,
        _ => 0,
    });
    // o(10).uid should now be NULL; o(11).uid stays 2.
    assert_eq!(rows[0], vec![Value::Int(10), Value::Null]);
    assert_eq!(rows[1], vec![Value::Int(11), Value::Int(2)]);
}

#[test]
fn set_null_rejected_when_child_column_is_not_null() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        // uid is NOT NULL — SET NULL would violate; plan should
        // reject.
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL, \
         FOREIGN KEY (uid) REFERENCES u(id) ON DELETE SET NULL)",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    let r = eng.execute("DELETE FROM u WHERE id = 1");
    assert!(
        matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("SET NULL") && s.contains("NOT NULL")),
        "got {r:?}"
    );
    // Nothing changed.
    assert_eq!(select_rows(&mut eng, "SELECT id FROM u").len(), 1);
}

#[test]
fn set_default_writes_default_into_child_fk_column() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT DEFAULT 999 NOT NULL, \
         FOREIGN KEY (uid) REFERENCES u(id) ON DELETE SET DEFAULT)",
        "INSERT INTO u VALUES (1), (999)",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    eng.execute("DELETE FROM u WHERE id = 1").unwrap();
    let rows = select_rows(&mut eng, "SELECT uid FROM o");
    assert_eq!(rows[0], vec![Value::Int(999)]);
}

#[test]
fn set_default_rejected_when_child_has_no_default() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT, \
         FOREIGN KEY (uid) REFERENCES u(id) ON DELETE SET DEFAULT)",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    let r = eng.execute("DELETE FROM u WHERE id = 1");
    assert!(
        matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("no DEFAULT")),
        "got {r:?}"
    );
}

#[test]
fn set_null_skips_rows_with_already_null_fk_value() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT, \
         FOREIGN KEY (uid) REFERENCES u(id) ON DELETE SET NULL)",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (10, 1), (11, NULL)",
    ]);
    eng.execute("DELETE FROM u WHERE id = 1").unwrap();
    let rows = select_rows(&mut eng, "SELECT uid FROM o ORDER BY id");
    // o(10).uid → NULL (was 1); o(11).uid stays NULL.
    assert_eq!(rows[0], vec![Value::Null]);
    assert_eq!(rows[1], vec![Value::Null]);
}
