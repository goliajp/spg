//! End-to-end engine tests. Drives the full parse → plan → execute chain
//! through `Engine::execute` and asserts on the visible behaviour. The
//! v0.3 chain (CREATE → INSERT → SELECT *) is here; v0.4 widens it to
//! WHERE filtering, projection, and table aliases.

use spg_engine::eval::EvalError;
use spg_engine::{Engine, EngineError, QueryResult};
use spg_storage::{ColumnSchema, DataType, Row, Value};

fn unwrap_rows(r: QueryResult) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    match r {
        QueryResult::Rows { columns, rows } => (columns, rows),
        QueryResult::CommandOk { .. } => panic!("expected Rows"),
        _ => panic!("unexpected QueryResult variant"),
    }
}

#[test]
fn create_two_inserts_then_select_returns_two_rows() {
    let mut engine = Engine::new();

    let r = engine
        .execute("CREATE TABLE accounts (id BIGINT NOT NULL, owner TEXT NOT NULL, balance FLOAT)")
        .expect("create table");
    match r {
        QueryResult::CommandOk { affected, .. } => assert_eq!(affected, 0),
        QueryResult::Rows { .. } => panic!("expected CommandOk"),
        _ => panic!("unexpected QueryResult variant"),
    }

    engine
        .execute("INSERT INTO accounts VALUES (1, 'alice', 100.5)")
        .expect("insert #1");
    engine
        .execute("INSERT INTO accounts VALUES (2, 'bob', NULL)")
        .expect("insert #2");

    let (columns, rows) = unwrap_rows(engine.execute("SELECT * FROM accounts").unwrap());
    assert_eq!(columns.len(), 3);
    assert_eq!(columns[0].name, "id");
    assert_eq!(columns[0].ty, DataType::BigInt);
    assert!(!columns[0].nullable);
    assert_eq!(columns[2].ty, DataType::Float);
    assert!(columns[2].nullable);

    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].values,
        vec![Value::BigInt(1), Value::text("alice"), Value::Float(100.5),]
    );
    assert_eq!(
        rows[1].values,
        vec![Value::BigInt(2), Value::text("bob"), Value::Null]
    );
}

#[test]
fn engine_state_persists_across_execute_calls() {
    let mut engine = Engine::new();
    engine.execute("CREATE TABLE t (x INT NOT NULL)").unwrap();
    for _ in 0..5 {
        engine.execute("INSERT INTO t VALUES (1)").unwrap();
    }
    let (_, rows) = unwrap_rows(engine.execute("SELECT * FROM t").unwrap());
    assert_eq!(rows.len(), 5);
}

#[test]
fn trailing_semicolons_per_statement_accepted() {
    let mut engine = Engine::new();
    engine.execute("CREATE TABLE t (x INT);").unwrap();
    engine.execute("INSERT INTO t VALUES (1);").unwrap();
    let (_, rows) = unwrap_rows(engine.execute("SELECT * FROM t;").unwrap());
    assert_eq!(rows.len(), 1);
}

// --- v0.4 end-to-end -------------------------------------------------------

fn populate_orders(e: &mut Engine) {
    e.execute("CREATE TABLE orders (id INT NOT NULL, status TEXT NOT NULL, total FLOAT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO orders VALUES (1, 'paid', 120.0)")
        .unwrap();
    e.execute("INSERT INTO orders VALUES (2, 'unpaid', 30.0)")
        .unwrap();
    e.execute("INSERT INTO orders VALUES (3, 'paid', 200.0)")
        .unwrap();
    e.execute("INSERT INTO orders VALUES (4, 'cancelled', 0.0)")
        .unwrap();
}

#[test]
fn where_filter_combined_predicates() {
    let mut engine = Engine::new();
    populate_orders(&mut engine);
    let r = engine
        .execute("SELECT * FROM orders WHERE status = 'paid' AND total > 100")
        .unwrap();
    let (_, rows) = unwrap_rows(r);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[0], Value::Int(1));
    assert_eq!(rows[1].values[0], Value::Int(3));
}

#[test]
fn projection_with_alias_and_filter() {
    let mut engine = Engine::new();
    populate_orders(&mut engine);
    let r = engine
        .execute("SELECT id AS order_id, total AS amount FROM orders WHERE total > 50")
        .unwrap();
    let (cols, rows) = unwrap_rows(r);
    assert_eq!(cols[0].name, "order_id");
    assert_eq!(cols[1].name, "amount");
    assert_eq!(rows.len(), 2);
}

#[test]
fn table_alias_used_in_where_and_projection() {
    let mut engine = Engine::new();
    populate_orders(&mut engine);
    let r = engine
        .execute("SELECT o.id, o.status FROM orders AS o WHERE o.total >= 100")
        .unwrap();
    let (cols, rows) = unwrap_rows(r);
    assert_eq!(cols.len(), 2);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[1], Value::text("paid"));
}

#[test]
fn null_is_excluded_by_where() {
    // status NULL would fail the equality predicate as NULL → filtered out.
    let mut engine = Engine::new();
    engine
        .execute("CREATE TABLE t (id INT NOT NULL, status TEXT)")
        .unwrap();
    engine.execute("INSERT INTO t VALUES (1, 'on')").unwrap();
    engine.execute("INSERT INTO t VALUES (2, NULL)").unwrap();
    engine.execute("INSERT INTO t VALUES (3, 'on')").unwrap();
    let (_, rows) = unwrap_rows(
        engine
            .execute("SELECT id FROM t WHERE status = 'on'")
            .unwrap(),
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[0], Value::Int(1));
    assert_eq!(rows[1].values[0], Value::Int(3));
}

#[test]
fn unknown_qualifier_propagates_from_eval() {
    let mut engine = Engine::new();
    engine.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    engine.execute("INSERT INTO t VALUES (1)").unwrap();
    let err = engine.execute("SELECT bogus.id FROM t").unwrap_err();
    assert!(matches!(
        err,
        EngineError::Eval(EvalError::UnknownQualifier { ref qualifier, .. })
            if qualifier == "bogus"
    ));
}
