//! v7.17.0 Phase 3.P0-59 — SHOW CREATE TABLE <t>.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn show_create_table_returns_ddl() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL, PRIMARY KEY (id))")
        .unwrap();
    let r = rows(e.execute("SHOW CREATE TABLE users").unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::text("users"));
    let ddl = match &r[0][1] {
        Value::Text(s) => s.to_string(),
        _ => panic!(),
    };
    assert!(ddl.contains("CREATE TABLE"));
    assert!(ddl.contains("users"));
    assert!(ddl.contains("id"));
    assert!(ddl.contains("PRIMARY KEY"));
}

#[test]
fn show_create_table_includes_foreign_key() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE parents (id INT NOT NULL, PRIMARY KEY (id))")
        .unwrap();
    e.execute(
        "CREATE TABLE children (id INT NOT NULL, parent_id INT NOT NULL, \
         FOREIGN KEY (parent_id) REFERENCES parents (id))",
    )
    .unwrap();
    let r = rows(e.execute("SHOW CREATE TABLE children").unwrap());
    let ddl = match &r[0][1] {
        Value::Text(s) => s.to_string(),
        _ => panic!(),
    };
    assert!(ddl.contains("FOREIGN KEY"));
    assert!(ddl.contains("parents"));
}

#[test]
fn show_create_table_errors_on_missing_table() {
    let mut e = Engine::new();
    let r = e.execute("SHOW CREATE TABLE nonexistent");
    assert!(r.is_err());
}
