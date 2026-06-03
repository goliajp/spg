//! v7.9.4 — INSERT / UPDATE / DELETE RETURNING. mailrs migration
//! blocker #1.

use spg_engine::{Engine, QueryResult};
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

fn rows_of(eng: &mut Engine, sql: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { columns, rows } => (
            columns.iter().map(|c| c.name.clone()).collect(),
            rows.into_iter().map(|r| r.values).collect(),
        ),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn insert_returning_single_column() {
    let mut eng = engine_with(&[
        "CREATE TABLE api_keys (id INT NOT NULL, label TEXT)",
    ]);
    let (cols, vals) = rows_of(
        &mut eng,
        "INSERT INTO api_keys VALUES (1, 'production') RETURNING id",
    );
    assert_eq!(vals.len(), 1);
    assert_eq!(vals[0], vec![Value::Int(1)]);
    assert_eq!(cols.len(), 1);
}

#[test]
fn insert_returning_star() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL, name TEXT NOT NULL)",
    ]);
    let (cols, vals) = rows_of(
        &mut eng,
        "INSERT INTO u VALUES (10, 'alice') RETURNING *",
    );
    assert_eq!(vals.len(), 1);
    assert_eq!(vals[0], vec![Value::Int(10), Value::Text("alice".into())]);
    assert_eq!(cols.len(), 2);
}

#[test]
fn insert_returning_multi_row() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
    ]);
    let (_cols, vals) = rows_of(
        &mut eng,
        "INSERT INTO t VALUES (1), (2), (3) RETURNING id",
    );
    assert_eq!(vals.len(), 3);
    assert_eq!(vals[0][0], Value::Int(1));
    assert_eq!(vals[1][0], Value::Int(2));
    assert_eq!(vals[2][0], Value::Int(3));
}

#[test]
fn insert_returning_aliased_expr() {
    // mailrs IMAP UID allocation pattern: `RETURNING expr AS alias`.
    let mut eng = engine_with(&[
        "CREATE TABLE mb (id INT NOT NULL, uidnext INT NOT NULL)",
    ]);
    let (cols, vals) = rows_of(
        &mut eng,
        "INSERT INTO mb VALUES (1, 100) RETURNING id, uidnext - 1 AS prev_uid",
    );
    assert_eq!(vals.len(), 1);
    assert_eq!(cols, vec!["?column?".to_string(), "prev_uid".to_string()]);
    assert_eq!(vals[0], vec![Value::Int(1), Value::Int(99)]);
}

#[test]
fn update_returning_new_values() {
    let mut eng = engine_with(&[
        "CREATE TABLE mb (id INT NOT NULL, uidnext INT NOT NULL)",
        "INSERT INTO mb VALUES (1, 100)",
    ]);
    let (_cols, vals) = rows_of(
        &mut eng,
        "UPDATE mb SET uidnext = uidnext + 5 WHERE id = 1 \
         RETURNING id, uidnext - 5 AS base_uid, uidnext AS new_uidnext",
    );
    assert_eq!(vals.len(), 1);
    assert_eq!(vals[0][0], Value::Int(1));
    assert_eq!(vals[0][1], Value::Int(100));
    assert_eq!(vals[0][2], Value::Int(105));
}

#[test]
fn delete_returning_pre_delete_state() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, payload TEXT)",
        "INSERT INTO t VALUES (1, 'a'), (2, 'b')",
    ]);
    let (_cols, vals) = rows_of(
        &mut eng,
        "DELETE FROM t WHERE id = 1 RETURNING id, payload",
    );
    assert_eq!(vals.len(), 1);
    assert_eq!(vals[0], vec![Value::Int(1), Value::Text("a".into())]);
    // Sanity: actually gone.
    let after = match eng.execute("SELECT id FROM t").unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => 0,
    };
    assert_eq!(after, 1);
}

#[test]
fn insert_without_returning_still_returns_command_ok() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
    ]);
    let r = eng.execute("INSERT INTO t VALUES (1)").unwrap();
    assert!(matches!(r, QueryResult::CommandOk { affected: 1, .. }));
}
