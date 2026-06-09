//! v7.17.0 Phase 3.P0-58 — `SHOW DATABASES` / `SHOW SCHEMAS`.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn show_databases_returns_canonical_mysql_set() {
    let mut e = Engine::new();
    let r = rows(e.execute("SHOW DATABASES").unwrap());
    let names: Vec<String> = r
        .into_iter()
        .map(|row| match row[0].clone() {
            Value::Text(s) => s,
            _ => panic!(),
        })
        .collect();
    assert!(names.contains(&"information_schema".to_string()));
    assert!(names.contains(&"mysql".to_string()));
    assert!(names.contains(&"performance_schema".to_string()));
    assert!(names.contains(&"sys".to_string()));
    assert!(names.contains(&"postgres".to_string()));
}

#[test]
fn show_schemas_is_alias() {
    let mut e = Engine::new();
    let r = rows(e.execute("SHOW SCHEMAS").unwrap());
    assert_eq!(r.len(), 5);
}
