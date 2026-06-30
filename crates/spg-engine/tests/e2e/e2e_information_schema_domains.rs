//! v7.37.24 (24.2) — information_schema.domains view. Liquibase
//! / Alembic migration tools read this to round-trip DOMAIN
//! types across the dump/restore cycle.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter().map(|r| r.values).collect()
}

#[test]
fn information_schema_domains_lists_user_domains() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN positive_int AS INT CHECK (VALUE > 0)")
        .unwrap();
    e.execute("CREATE DOMAIN short_text AS VARCHAR(50)").unwrap();
    let rs = rows(&mut e, "SELECT * FROM information_schema.domains");
    assert_eq!(rs.len(), 2, "got {rs:?}");
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
    assert!(names.contains(&"positive_int".to_string()));
    assert!(names.contains(&"short_text".to_string()));
}

#[test]
fn information_schema_domains_carries_base_type() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN positive_int AS INT").unwrap();
    let rs = rows(&mut e, "SELECT * FROM information_schema.domains");
    // Position 3 = data_type.
    let r = rs
        .iter()
        .find(|r| matches!(&r[2], Value::Text(s) if s.as_ref() == "positive_int"))
        .unwrap();
    if let Value::Text(s) = &r[3] {
        // PG renders INT as 'integer' for data_type.
        assert!(
            s.contains("int") || s.contains("integer"),
            "data_type for INT base: {s:?}"
        );
    } else {
        panic!("data_type wrong type");
    }
}

#[test]
fn information_schema_domains_empty_when_no_domains() {
    let mut e = Engine::new();
    let rs = rows(&mut e, "SELECT * FROM information_schema.domains");
    assert!(rs.is_empty(), "got {rs:?}");
}

#[test]
fn information_schema_domains_emits_pg_canonical_columns() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM information_schema.domains")
        .unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "domain_catalog",
        "domain_schema",
        "domain_name",
        "data_type",
        "udt_catalog",
        "udt_schema",
        "udt_name",
        "domain_default",
        "is_nullable",
    ] {
        assert!(
            names.contains(&must),
            "missing column {must}, got {names:?}"
        );
    }
}
