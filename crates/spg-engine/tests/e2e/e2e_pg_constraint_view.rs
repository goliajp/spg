//! v7.17.0 Phase 3.P0-54 — pg_catalog.pg_constraint view.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn pg_constraint_lists_primary_key() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL, PRIMARY KEY (id))")
        .unwrap();
    let r = rows(
        e.execute(
            "SELECT contype, conkey FROM pg_catalog.pg_constraint \
             WHERE conrelid = 't' AND contype = 'p'",
        )
        .unwrap(),
    );
    assert!(!r.is_empty());
    assert_eq!(r[0][0], Value::text("p"));
    assert_eq!(r[0][1], Value::text("id"));
}

#[test]
fn pg_constraint_lists_foreign_key() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE parents (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    e.execute(
        "CREATE TABLE children (id INT NOT NULL, parent_id INT NOT NULL, \
         FOREIGN KEY (parent_id) REFERENCES parents (id))",
    )
    .unwrap();
    let r = rows(
        e.execute(
            "SELECT contype, conrelid, confrelid, conkey, confkey \
             FROM pg_catalog.pg_constraint WHERE contype = 'f'",
        )
        .unwrap(),
    );
    assert!(!r.is_empty());
    assert_eq!(r[0][0], Value::text("f"));
    assert_eq!(r[0][1], Value::text("children"));
    assert_eq!(r[0][2], Value::text("parents"));
    assert_eq!(r[0][3], Value::text("parent_id"));
    assert_eq!(r[0][4], Value::text("id"));
}

#[test]
fn pg_constraint_lists_composite_unique() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT NOT NULL, b INT NOT NULL, UNIQUE (a, b))")
        .unwrap();
    let r = rows(
        e.execute(
            "SELECT contype, conkey FROM pg_catalog.pg_constraint \
             WHERE conrelid = 't' AND contype = 'u'",
        )
        .unwrap(),
    );
    assert!(!r.is_empty());
    assert_eq!(r[0][0], Value::text("u"));
    assert_eq!(r[0][1], Value::text("a,b"));
}
