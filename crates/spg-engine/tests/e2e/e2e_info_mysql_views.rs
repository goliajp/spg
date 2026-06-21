//! v7.17.0 Phase 3.P0-63/64 — information_schema.{KEY_COLUMN_USAGE,
//! REFERENTIAL_CONSTRAINTS, STATISTICS, ROUTINES}.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn key_column_usage_lists_fk_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE parents (id INT NOT NULL, PRIMARY KEY (id))")
        .unwrap();
    e.execute(
        "CREATE TABLE children (id INT NOT NULL, parent_id INT NOT NULL, \
         FOREIGN KEY (parent_id) REFERENCES parents (id))",
    )
    .unwrap();
    let r = rows(
        e.execute(
            "SELECT COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME \
             FROM information_schema.KEY_COLUMN_USAGE \
             WHERE TABLE_NAME = 'children' AND REFERENCED_TABLE_NAME = 'parents'",
        )
        .unwrap(),
    );
    assert!(!r.is_empty());
    assert_eq!(r[0][0], Value::text("parent_id"));
    assert_eq!(r[0][1], Value::text("parents"));
    assert_eq!(r[0][2], Value::text("id"));
}

#[test]
fn referential_constraints_lists_fk() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE parents (id INT NOT NULL, PRIMARY KEY (id))")
        .unwrap();
    e.execute(
        "CREATE TABLE children (id INT NOT NULL, parent_id INT NOT NULL, \
         FOREIGN KEY (parent_id) REFERENCES parents (id))",
    )
    .unwrap();
    let r = rows(
        e.execute(
            "SELECT TABLE_NAME, REFERENCED_TABLE_NAME \
             FROM information_schema.REFERENTIAL_CONSTRAINTS",
        )
        .unwrap(),
    );
    assert!(!r.is_empty());
    assert_eq!(r[0][0], Value::text("children"));
    assert_eq!(r[0][1], Value::text("parents"));
}

#[test]
fn statistics_lists_per_index_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX idx_t_name ON t (name)").unwrap();
    let r = rows(
        e.execute(
            "SELECT TABLE_NAME, INDEX_NAME, COLUMN_NAME \
             FROM information_schema.STATISTICS WHERE INDEX_NAME = 'idx_t_name'",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::text("t"));
    assert_eq!(r[0][1], Value::text("idx_t_name"));
    assert_eq!(r[0][2], Value::text("name"));
}

#[test]
fn routines_is_empty() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT COUNT(*) FROM information_schema.ROUTINES")
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::BigInt(0));
}
