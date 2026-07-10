//! v7.37.24 (24.1) — `pg_catalog.pg_enum` view. ORM enum codecs
//! (sqlx, Diesel, sea-orm) and pg_dump's enum-by-label query
//! read this surface to reconstruct ENUM types at dump-time.

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
fn pg_enum_lists_every_label_per_type() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')")
        .unwrap();
    e.execute("CREATE TYPE color AS ENUM ('red', 'green', 'blue')")
        .unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_enum");
    // 3 + 3 = 6 rows.
    assert_eq!(rs.len(), 6, "got {rs:?}");
    let labels: Vec<String> = rs
        .iter()
        .filter_map(|r| {
            if let Value::Text(s) = &r[3] {
                Some(s.to_string())
            } else {
                None
            }
        })
        .collect();
    for must in ["sad", "ok", "happy", "red", "green", "blue"] {
        assert!(labels.contains(&must.to_string()), "missing {must}");
    }
}

#[test]
fn pg_enum_enumsortorder_increments_per_type() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE size AS ENUM ('s', 'm', 'l', 'xl')")
        .unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_enum");
    // enumsortorder at position 2 should be 1.0, 2.0, 3.0, 4.0.
    let mut orders: Vec<f64> = rs
        .iter()
        .filter_map(|r| match r[2] {
            Value::Float(f) => Some(f),
            _ => None,
        })
        .collect();
    orders.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert_eq!(orders, vec![1.0, 2.0, 3.0, 4.0]);
}

#[test]
fn pg_enum_empty_when_no_enums() {
    let mut e = Engine::new();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_enum");
    assert!(rs.is_empty(), "got {rs:?}");
}

#[test]
fn pg_enum_enumtypid_groups_labels_of_same_type() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE a AS ENUM ('x', 'y')").unwrap();
    e.execute("CREATE TYPE b AS ENUM ('m', 'n', 'o')").unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_enum");
    // Position 1 = enumtypid. Group by it; expect one group of
    // size 2 and one group of size 3.
    let mut counts: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for r in &rs {
        if let Value::BigInt(typid) = r[1] {
            *counts.entry(typid).or_default() += 1;
        }
    }
    let mut sizes: Vec<usize> = counts.values().copied().collect();
    sizes.sort();
    assert_eq!(sizes, vec![2, 3]);
}
