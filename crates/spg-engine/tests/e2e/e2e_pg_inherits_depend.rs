//! v7.37.24 (24.16 + 24.17) — pg_inherits + pg_depend views.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("Rows");
    };
    rows.into_iter().map(|r| r.values).collect()
}

#[test]
fn pg_inherits_emits_pg_canonical_columns() {
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_catalog.pg_inherits").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in ["inhrelid", "inhparent", "inhseqno", "inhdetachpending"] {
        assert!(
            names.contains(&must),
            "pg_inherits missing {must}: {names:?}"
        );
    }
}

#[test]
fn pg_inherits_lists_partition_children() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c_l (id BIGINT, region TEXT) PARTITION BY LIST (region)")
        .unwrap();
    e.execute("CREATE TABLE c_jp PARTITION OF c_l FOR VALUES IN ('jp')")
        .unwrap();
    e.execute("CREATE TABLE c_kr PARTITION OF c_l FOR VALUES IN ('kr')")
        .unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_inherits");
    // Two partition children → two rows.
    assert_eq!(rs.len(), 2, "got {rs:?}");
    // v7.39 (round 642) — inhseqno is the PARENT's position in the
    // CHILD's parent list, not the child's index among its siblings.
    // This asserted 1, 2. Measured on PG18: two partitions of one parent
    // both read 1, and only a child of two parents gets 1 and 2.
    let seqs: Vec<i32> = rs
        .iter()
        .filter_map(|r| match r[2] {
            Value::Int(n) => Some(n),
            _ => None,
        })
        .collect();
    assert_eq!(seqs, vec![1, 1]);
}

#[test]
fn pg_inherits_empty_when_no_partitions() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE plain (id INT)").unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_inherits");
    assert!(rs.is_empty());
}

#[test]
fn pg_depend_returns_empty_with_pg_columns() {
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_catalog.pg_depend").unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "classid",
        "objid",
        "objsubid",
        "refclassid",
        "refobjid",
        "refobjsubid",
        "deptype",
    ] {
        assert!(names.contains(&must), "pg_depend missing {must}: {names:?}");
    }
    assert!(rows.is_empty());
}
