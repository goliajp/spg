//! v6.5.4 — DDL introspection: spg_table_ddl, spg_role_ddl,
//! spg_database_ddl virtual tables.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(res: QueryResult) -> Vec<Vec<Value<'static>>> {
    match res {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn table_ddl_round_trips_through_execute() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)")
        .unwrap();
    eng.execute("INSERT INTO t VALUES (1, 'a')").unwrap();

    let res = eng.execute("SELECT * FROM spg_table_ddl").unwrap();
    let got = rows_of(res);
    let t_row = got
        .iter()
        .find(|r| r[0] == Value::text("t"))
        .expect("t row");
    let ddl = match &t_row[1] {
        Value::Text(s) => s.to_string(),
        other => panic!("expected text ddl, got {other:?}"),
    };

    // Round-trip: drop the original table and recreate from the DDL.
    let mut eng2 = Engine::new();
    eng2.execute(&ddl).unwrap();
    // Schema must match: SELECT * FROM the recreated table returns
    // the right column shape (we INSERT then SELECT to confirm).
    eng2.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    let r = eng2.execute("SELECT id, name FROM t").unwrap();
    let rows = rows_of(r);
    assert_eq!(rows.len(), 1);
}

#[test]
fn role_ddl_round_trips() {
    let mut eng = Engine::new();
    eng.execute("CREATE USER alice WITH PASSWORD 'pw' ROLE 'admin'")
        .unwrap();

    let res = eng.execute("SELECT * FROM spg_role_ddl").unwrap();
    let got = rows_of(res);
    let alice_row = got
        .iter()
        .find(|r| r[0] == Value::text("alice"))
        .expect("alice row");
    let ddl = match &alice_row[1] {
        Value::Text(s) => s.to_string(),
        other => panic!("expected text ddl, got {other:?}"),
    };
    assert!(ddl.contains("CREATE USER alice"));
    assert!(ddl.contains("ROLE 'admin'"));
    assert!(ddl.contains("'<redacted>'"));
}

#[test]
fn database_ddl_includes_tables_and_users() {
    let mut eng = Engine::new();
    eng.execute("CREATE USER bob WITH PASSWORD 'pw' ROLE 'readonly'")
        .unwrap();
    eng.execute("CREATE TABLE t (id INT)").unwrap();
    eng.execute("CREATE TABLE u (k TEXT NOT NULL)").unwrap();

    let res = eng.execute("SELECT * FROM spg_database_ddl").unwrap();
    let got = rows_of(res);
    assert_eq!(got.len(), 1);
    let ddl = match &got[0][0] {
        Value::Text(s) => s.to_string(),
        other => panic!("expected text ddl, got {other:?}"),
    };
    assert!(ddl.contains("CREATE USER bob"));
    assert!(ddl.contains("CREATE TABLE t"));
    assert!(ddl.contains("CREATE TABLE u"));
    assert!(ddl.contains("NOT NULL"));
}
