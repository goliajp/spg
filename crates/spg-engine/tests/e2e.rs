//! End-to-end engine test: drive the full CREATE → INSERT → SELECT chain
//! through `Engine::execute`. This is the v0.3 acceptance gate that the
//! quality-gate step then runs ten times to check for flakiness.

use spg_engine::{Engine, QueryResult};
use spg_storage::{DataType, Value};

#[test]
fn create_two_inserts_then_select_returns_two_rows() {
    let mut engine = Engine::new();

    let r = engine
        .execute("CREATE TABLE accounts (id BIGINT NOT NULL, owner TEXT NOT NULL, balance FLOAT)")
        .expect("create table");
    match r {
        QueryResult::CommandOk { affected } => assert_eq!(affected, 0),
        QueryResult::Rows { .. } => panic!("expected CommandOk"),
    }

    engine
        .execute("INSERT INTO accounts VALUES (1, 'alice', 100.5)")
        .expect("insert #1");
    engine
        .execute("INSERT INTO accounts VALUES (2, 'bob', NULL)")
        .expect("insert #2");

    let r = engine
        .execute("SELECT * FROM accounts")
        .expect("select star");
    match r {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns.len(), 3);
            assert_eq!(columns[0].name, "id");
            assert_eq!(columns[0].ty, DataType::BigInt);
            assert!(!columns[0].nullable);
            assert_eq!(columns[2].ty, DataType::Float);
            assert!(columns[2].nullable);

            assert_eq!(rows.len(), 2);
            assert_eq!(
                rows[0].values,
                vec![
                    Value::BigInt(1),
                    Value::Text("alice".into()),
                    Value::Float(100.5),
                ]
            );
            assert_eq!(
                rows[1].values,
                vec![Value::BigInt(2), Value::Text("bob".into()), Value::Null,]
            );
        }
        QueryResult::CommandOk { .. } => panic!("expected Rows"),
    }
}

#[test]
fn engine_state_persists_across_execute_calls() {
    // Idempotence-ish: running the same INSERT twice accumulates rows.
    let mut engine = Engine::new();
    engine.execute("CREATE TABLE t (x INT NOT NULL)").unwrap();
    for _ in 0..5 {
        engine.execute("INSERT INTO t VALUES (1)").unwrap();
    }
    let QueryResult::Rows { rows, .. } = engine.execute("SELECT * FROM t").unwrap() else {
        panic!()
    };
    assert_eq!(rows.len(), 5);
}

#[test]
fn trailing_semicolons_per_statement_accepted() {
    let mut engine = Engine::new();
    engine.execute("CREATE TABLE t (x INT);").unwrap();
    engine.execute("INSERT INTO t VALUES (1);").unwrap();
    let QueryResult::Rows { rows, .. } = engine.execute("SELECT * FROM t;").unwrap() else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
}
