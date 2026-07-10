//! v7.37.24 (24.13 + 24.14) — pg_am + pg_collation views.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

#[test]
fn pg_am_lists_heap_and_btree() {
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_catalog.pg_am").unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in ["oid", "amname", "amhandler", "amtype"] {
        assert!(names.contains(&must), "pg_am missing {must}: {names:?}");
    }
    let amnames: Vec<String> = rows
        .iter()
        .filter_map(|r| {
            if let Value::Text(s) = &r.values[1] {
                Some(s.to_string())
            } else {
                None
            }
        })
        .collect();
    assert!(amnames.contains(&"heap".to_string()));
    assert!(amnames.contains(&"btree".to_string()));
}

#[test]
fn pg_am_carries_pg_canonical_oids() {
    // PG hard-codes oid 2 = heap, oid 403 = btree.
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_catalog.pg_am").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("Rows");
    };
    let oids: Vec<i64> = rows
        .iter()
        .filter_map(|r| {
            if let Value::BigInt(o) = r.values[0] {
                Some(o)
            } else {
                None
            }
        })
        .collect();
    assert!(oids.contains(&2), "heap OID 2 missing");
    assert!(oids.contains(&403), "btree OID 403 missing");
}

#[test]
fn pg_collation_lists_default_c_posix() {
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_catalog.pg_collation").unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "oid",
        "collname",
        "collnamespace",
        "collowner",
        "collprovider",
        "collisdeterministic",
        "collencoding",
        "collcollate",
        "collctype",
    ] {
        assert!(
            names.contains(&must),
            "pg_collation missing {must}: {names:?}"
        );
    }
    let collnames: Vec<String> = rows
        .iter()
        .filter_map(|r| {
            if let Value::Text(s) = &r.values[1] {
                Some(s.to_string())
            } else {
                None
            }
        })
        .collect();
    for must in ["default", "C", "POSIX"] {
        assert!(
            collnames.contains(&must.to_string()),
            "missing {must}: {collnames:?}"
        );
    }
}

#[test]
fn pg_collation_carries_pg_canonical_oids() {
    // PG hard-codes oid 100 = default, 950 = C, 951 = POSIX.
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_catalog.pg_collation").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("Rows");
    };
    let oids: Vec<i64> = rows
        .iter()
        .filter_map(|r| {
            if let Value::BigInt(o) = r.values[0] {
                Some(o)
            } else {
                None
            }
        })
        .collect();
    for must in [100i64, 950, 951] {
        assert!(oids.contains(&must), "missing oid {must}: {oids:?}");
    }
}
