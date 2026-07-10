//! v7.37.24 (24.9) — three new information_schema views:
//! schemata, views, table_constraints. Liquibase / Flyway /
//! Alembic / sqlx introspection all query these surfaces for
//! schema-drift detection.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter().map(|r| r.values).collect()
}

#[test]
fn information_schema_schemata_lists_public_pg_catalog_info() {
    let mut e = Engine::new();
    let rs = rows(&mut e, "SELECT * FROM information_schema.schemata");
    // Three schemas: public, pg_catalog, information_schema.
    let names: Vec<String> = rs
        .iter()
        .filter_map(|r| {
            if let Value::Text(s) = &r[1] {
                Some(s.to_string())
            } else {
                None
            }
        })
        .collect();
    assert!(names.contains(&"public".to_string()), "got {names:?}");
    assert!(names.contains(&"pg_catalog".to_string()), "got {names:?}");
    assert!(
        names.contains(&"information_schema".to_string()),
        "got {names:?}"
    );
}

#[test]
fn information_schema_views_lists_user_views() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, status TEXT)").unwrap();
    e.execute("CREATE VIEW paid_t AS SELECT * FROM t WHERE status = 'paid'")
        .unwrap();
    let rs = rows(&mut e, "SELECT * FROM information_schema.views");
    let names: Vec<String> = rs
        .iter()
        .filter_map(|r| {
            if let Value::Text(s) = &r[2] {
                Some(s.to_string())
            } else {
                None
            }
        })
        .collect();
    assert!(names.contains(&"paid_t".to_string()), "got {names:?}");
    // view_definition (position 3) carries the SELECT body.
    let v = rs
        .iter()
        .find(|r| matches!(&r[2], Value::Text(s) if s.as_ref() == "paid_t"))
        .unwrap();
    if let Value::Text(body) = &v[3] {
        assert!(body.contains("SELECT"), "view_definition: {body:?}");
    } else {
        panic!("view_definition wrong type");
    }
}

#[test]
fn information_schema_table_constraints_lists_pk_uk_fk_check() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE p (id INT NOT NULL PRIMARY KEY, status TEXT NOT NULL, \
         CHECK (status IN ('a', 'b')), UNIQUE (status))",
    )
    .unwrap();
    e.execute(
        "CREATE TABLE c (id INT NOT NULL, pid INT NOT NULL, \
         FOREIGN KEY (pid) REFERENCES p(id))",
    )
    .unwrap();
    let rs = rows(&mut e, "SELECT * FROM information_schema.table_constraints");
    // Position 6 = constraint_type.
    let types: Vec<String> = rs
        .iter()
        .filter_map(|r| {
            if let Value::Text(s) = &r[6] {
                Some(s.to_string())
            } else {
                None
            }
        })
        .collect();
    assert!(
        types.contains(&"PRIMARY KEY".to_string()),
        "missing PK: {types:?}"
    );
    assert!(
        types.contains(&"UNIQUE".to_string()),
        "missing UK: {types:?}"
    );
    assert!(
        types.contains(&"FOREIGN KEY".to_string()),
        "missing FK: {types:?}"
    );
    assert!(
        types.contains(&"CHECK".to_string()),
        "missing CHECK: {types:?}"
    );
}

#[test]
fn information_schema_table_constraints_empty_when_no_user_tables() {
    let mut e = Engine::new();
    let rs = rows(&mut e, "SELECT * FROM information_schema.table_constraints");
    assert!(rs.is_empty(), "expected empty, got {rs:?}");
}
