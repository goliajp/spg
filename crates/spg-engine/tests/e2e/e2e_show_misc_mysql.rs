//! v7.17.0 Phase 3.P0-60..P0-62 — MySQL SHOW INDEXES / STATUS / VARIABLES / PROCESSLIST.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn show_indexes_lists_table_indexes() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX idx_t_name ON t (name)").unwrap();
    let r = rows(e.execute("SHOW INDEXES FROM t").unwrap());
    assert!(r.iter().any(|row| row[2] == Value::text("idx_t_name")));
}

#[test]
fn show_index_alias_works() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX idx_t_n ON t (name)").unwrap();
    let r = rows(e.execute("SHOW INDEX FROM t").unwrap());
    assert!(!r.is_empty());
}

#[test]
fn show_keys_alias_works() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX idx_t_n ON t (name)").unwrap();
    let r = rows(e.execute("SHOW KEYS FROM t").unwrap());
    assert!(!r.is_empty());
}

#[test]
fn show_status_returns_canonical_pairs() {
    let mut e = Engine::new();
    let r = rows(e.execute("SHOW STATUS").unwrap());
    let names: Vec<String> = r
        .iter()
        .map(|row| match &row[0] {
            Value::Text(s) => s.to_string(),
            _ => panic!(),
        })
        .collect();
    assert!(names.contains(&"Uptime".to_string()));
    assert!(names.contains(&"Threads_connected".to_string()));
}

#[test]
fn show_variables_returns_canonical_pairs() {
    let mut e = Engine::new();
    let r = rows(e.execute("SHOW VARIABLES").unwrap());
    let names: Vec<String> = r
        .iter()
        .map(|row| match &row[0] {
            Value::Text(s) => s.to_string(),
            _ => panic!(),
        })
        .collect();
    assert!(names.contains(&"version".to_string()));
    assert!(names.contains(&"character_set_server".to_string()));
}

#[test]
fn show_processlist_returns_self_row() {
    let mut e = Engine::new();
    let r = rows(e.execute("SHOW PROCESSLIST").unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][1], Value::text("postgres"));
}
