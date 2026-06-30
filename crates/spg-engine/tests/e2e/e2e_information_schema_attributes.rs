//! v7.37.24 (24.3) — information_schema.attributes view. Lists
//! every field of every composite type. PG-targeting ORM /
//! pg_dump path reads this to reconstruct CREATE TYPE … AS (…)
//! statements at dump-time.

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
fn information_schema_attributes_lists_fields_of_composite_type() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE addr AS (street TEXT, zip INT, country TEXT)")
        .unwrap();
    let rs = rows(&mut e, "SELECT * FROM information_schema.attributes");
    assert_eq!(rs.len(), 3, "got {rs:?}");
    // Position 3 = attribute_name, 4 = ordinal_position.
    let mut by_pos: Vec<(i32, String)> = rs
        .iter()
        .filter_map(|r| {
            let name = match &r[3] {
                Value::Text(s) => s.to_string(),
                _ => return None,
            };
            let pos = match r[4] {
                Value::Int(p) => p,
                _ => return None,
            };
            Some((pos, name))
        })
        .collect();
    by_pos.sort_by_key(|(p, _)| *p);
    assert_eq!(
        by_pos,
        vec![
            (1, "street".to_string()),
            (2, "zip".to_string()),
            (3, "country".to_string())
        ]
    );
}

#[test]
fn information_schema_attributes_carries_data_type_per_field() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE pair AS (a INT, b TEXT)").unwrap();
    let rs = rows(&mut e, "SELECT * FROM information_schema.attributes");
    // Position 5 = data_type.
    let a = rs
        .iter()
        .find(|r| matches!(&r[3], Value::Text(s) if s.as_ref() == "a"))
        .unwrap();
    let b = rs
        .iter()
        .find(|r| matches!(&r[3], Value::Text(s) if s.as_ref() == "b"))
        .unwrap();
    if let Value::Text(s) = &a[5] {
        assert!(s.contains("int"), "a's data_type: {s:?}");
    }
    if let Value::Text(s) = &b[5] {
        assert!(s == "text", "b's data_type: {s:?}");
    }
}

#[test]
fn information_schema_attributes_emits_pg_canonical_columns() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM information_schema.attributes")
        .unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "udt_catalog",
        "udt_schema",
        "udt_name",
        "attribute_name",
        "ordinal_position",
        "data_type",
        "is_nullable",
    ] {
        assert!(
            names.contains(&must),
            "missing column {must}, got {names:?}"
        );
    }
}

#[test]
fn information_schema_attributes_empty_when_no_composite_types() {
    let mut e = Engine::new();
    let rs = rows(&mut e, "SELECT * FROM information_schema.attributes");
    assert!(rs.is_empty(), "got {rs:?}");
}
