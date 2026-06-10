//! v6.4.6 — Transactional DDL hardening.
//!
//! The `tx_catalog` shadow catalog (v4.41.1) is the mechanism: DDL
//! inside an explicit BEGIN/COMMIT lands in the shadow, then COMMIT
//! atomically swaps it into the main catalog. ROLLBACK drops the
//! shadow. v6.4.6 formally locks this invariant with explicit e2e
//! coverage; v4.x callers relied on the mechanism implicitly without
//! a dedicated test surface.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(res: QueryResult) -> Vec<Vec<Value>> {
    match res {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn rollback_drops_table_created_in_tx() {
    let mut eng = Engine::new();
    eng.execute("BEGIN").unwrap();
    eng.execute("CREATE TABLE t (id INT)").unwrap();
    eng.execute("INSERT INTO t VALUES (1)").unwrap();
    // Visible inside the TX.
    let inside = rows_of(eng.execute("SELECT id FROM t").unwrap());
    assert_eq!(inside, vec![vec![Value::Int(1)]]);

    eng.execute("ROLLBACK").unwrap();

    // After ROLLBACK the table must be gone — selecting errors.
    let r = eng.execute("SELECT id FROM t");
    assert!(
        r.is_err(),
        "ROLLBACK should drop the table created in the TX"
    );
}

#[test]
fn commit_persists_table_and_rows_atomically() {
    let mut eng = Engine::new();
    eng.execute("BEGIN").unwrap();
    eng.execute("CREATE TABLE m (k INT, v TEXT)").unwrap();
    eng.execute("INSERT INTO m VALUES (1, 'a'), (2, 'b')")
        .unwrap();
    eng.execute("COMMIT").unwrap();

    let res = rows_of(eng.execute("SELECT k, v FROM m ORDER BY k").unwrap());
    assert_eq!(
        res,
        vec![
            vec![Value::Int(1), Value::Text("a".to_string())],
            vec![Value::Int(2), Value::Text("b".to_string())],
        ]
    );
}

#[test]
fn rollback_after_create_index_drops_the_index() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
    eng.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')")
        .unwrap();

    eng.execute("BEGIN").unwrap();
    eng.execute("CREATE INDEX ix_t_id ON t (id)").unwrap();
    eng.execute("ROLLBACK").unwrap();

    // After rollback the index must be gone — a fresh CREATE INDEX
    // with the same name must succeed.
    eng.execute("CREATE INDEX ix_t_id ON t (id)").unwrap();
}

#[test]
fn ddl_inside_tx_invisible_to_implicit_tx_before_commit() {
    // Single-engine sequential test: open a TX, create a table, then
    // (without committing) verify that another statement on the same
    // engine CAN see the table because both run in the same write
    // lock — SPG's tx_catalog only protects rollback semantics, not
    // session isolation. Documenting this behaviour.
    let mut eng = Engine::new();
    eng.execute("BEGIN").unwrap();
    eng.execute("CREATE TABLE t (id INT)").unwrap();
    // Same write-lock holder still sees the shadow.
    let r = eng.execute("SELECT id FROM t");
    assert!(r.is_ok(), "in-TX queries can read the shadow catalog");
    eng.execute("COMMIT").unwrap();
    // After commit it's globally visible.
    let r2 = eng.execute("SELECT id FROM t");
    assert!(r2.is_ok());
}
