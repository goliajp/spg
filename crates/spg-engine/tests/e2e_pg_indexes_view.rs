//! v7.17.0 Phase 3.P0-53 — pg_catalog.pg_indexes / pg_index views.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE users (id INT NOT NULL PRIMARY KEY, name TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX idx_users_name ON users (name)")
        .unwrap();
    e.execute("CREATE UNIQUE INDEX uq_users_name ON users (name)")
        .unwrap();
}

#[test]
fn pg_indexes_lists_user_index_by_table() {
    let mut e = Engine::new();
    setup(&mut e);
    let r = rows(
        e.execute(
            "SELECT indexname FROM pg_catalog.pg_indexes \
             WHERE tablename = 'users' AND schemaname = 'public' \
             ORDER BY indexname",
        )
        .unwrap(),
    );
    assert!(!r.is_empty());
    let names: Vec<_> = r
        .iter()
        .map(|row| match &row[0] {
            Value::Text(s) => s.clone(),
            _ => panic!(),
        })
        .collect();
    assert!(names.contains(&"idx_users_name".to_string()));
    assert!(names.contains(&"uq_users_name".to_string()));
}

#[test]
fn pg_indexes_indexdef_contains_create_index() {
    let mut e = Engine::new();
    setup(&mut e);
    let r = rows(
        e.execute(
            "SELECT indexdef FROM pg_catalog.pg_indexes \
             WHERE indexname = 'idx_users_name'",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    let def = match &r[0][0] {
        Value::Text(s) => s.clone(),
        _ => panic!(),
    };
    assert!(def.contains("CREATE"));
    assert!(def.contains("idx_users_name"));
    assert!(def.contains("name"));
}

#[test]
fn pg_index_raw_flags_unique() {
    let mut e = Engine::new();
    setup(&mut e);
    let r = rows(
        e.execute(
            "SELECT indisunique FROM pg_catalog.pg_index pi \
             JOIN pg_catalog.pg_indexes pix ON pi.indexrelid = pi.indexrelid \
             WHERE indisunique = TRUE",
        )
        .unwrap(),
    );
    assert!(!r.is_empty());
    assert_eq!(r[0][0], Value::Bool(true));
}
