//! v7.37.24 (24.8) — widened pg_class shape for monitoring /
//! dashboard / ORM-introspection compatibility. Asserts the
//! columns dashboards depend on (oid, relkind, reltuples,
//! relnatts, relhasindex, relhastriggers, relispartition).

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
fn pg_class_columns_match_pg_canonical() {
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_catalog.pg_class").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("expected Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "oid",
        "relname",
        "relnamespace",
        "relkind",
        "relnatts",
        "relhasindex",
        "relhastriggers",
        "relispartition",
        "reltuples",
        "relpages",
        "relpersistence",
    ] {
        assert!(
            names.contains(&must),
            "pg_class missing column {must}, got {names:?}"
        );
    }
}

#[test]
fn pg_class_relkind_p_for_partition_parent() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cust (id BIGINT, region TEXT) PARTITION BY LIST (region)")
        .unwrap();
    e.execute("CREATE TABLE cust_apac PARTITION OF cust FOR VALUES IN ('jp')")
        .unwrap();
    e.execute("CREATE TABLE plain (id INT)").unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_class");
    // Position 1 = relname, position 16 = relkind, position 26 = relispartition.
    let by_name = |needle: &str| {
        rs.iter()
            .find(|r| matches!(&r[1], Value::Text(s) if s.as_ref() == needle))
            .unwrap_or_else(|| panic!("missing pg_class row for {needle}"))
    };
    let cust = by_name("cust");
    assert!(
        matches!(&cust[16], Value::Text(s) if s.as_ref() == "p"),
        "parent relkind"
    );
    assert!(
        matches!(cust[26], Value::Bool(false)),
        "parent is not a partition"
    );
    let apac = by_name("cust_apac");
    assert!(matches!(apac[26], Value::Bool(true)), "apac is a partition");
    let plain = by_name("plain");
    assert!(
        matches!(&plain[16], Value::Text(s) if s.as_ref() == "r"),
        "plain table relkind"
    );
    assert!(
        matches!(plain[26], Value::Bool(false)),
        "plain not a partition"
    );
}

#[test]
fn pg_class_reltuples_reflects_row_count() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    for i in 0..10 {
        let sql = format!("INSERT INTO t VALUES ({i})");
        e.execute(&sql).unwrap();
    }
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_class");
    let t_row = rs
        .iter()
        .find(|r| matches!(&r[1], Value::Text(s) if s.as_ref() == "t"))
        .expect("missing pg_class row for t");
    // Position 10 = reltuples (Float).
    match t_row[10] {
        Value::Float(n) => assert!((n - 10.0).abs() < f64::EPSILON, "reltuples = 10"),
        ref other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn pg_class_relhasindex_after_create_index() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
    let r1 = rows(&mut e, "SELECT * FROM pg_catalog.pg_class");
    let t1 = r1
        .iter()
        .find(|r| matches!(&r[1], Value::Text(s) if s.as_ref() == "t"))
        .unwrap();
    // Position 13 = relhasindex.
    assert!(matches!(t1[13], Value::Bool(false)), "no index yet");
    e.execute("CREATE INDEX ix_t_name ON t(name)").unwrap();
    let r2 = rows(&mut e, "SELECT * FROM pg_catalog.pg_class");
    let t2 = r2
        .iter()
        .find(|r| matches!(&r[1], Value::Text(s) if s.as_ref() == "t"))
        .unwrap();
    assert!(matches!(t2[13], Value::Bool(true)), "has index");
}

// read01 — a pg_catalog view referenced only inside a subquery must
// still materialise. The `WHERE attrelid = (SELECT oid FROM pg_class
// WHERE relname = …)` shape is how ORMs / pg_dump introspect a table's
// columns; before the meta-view collector walked subqueries it failed
// with "__spg_pg_class does not exist".
#[test]
fn catalog_view_inside_subquery_materialises() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT NOT NULL)")
        .unwrap();
    // pg_attribute filtered by a pg_class subquery.
    let cols = rows(
        &mut e,
        "SELECT attname FROM pg_attribute \
         WHERE attrelid = (SELECT oid FROM pg_class WHERE relname = 't') \
           AND attnum > 0 ORDER BY attnum",
    );
    let names: Vec<String> = cols
        .iter()
        .filter_map(|r| match &r[0] {
            Value::Text(s) => Some(s.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["id", "name"]);

    // pg_index filtered by a pg_class subquery (no error / rows present).
    let idx = rows(
        &mut e,
        "SELECT indexrelid FROM pg_index \
         WHERE indrelid = (SELECT oid FROM pg_class WHERE relname = 't')",
    );
    assert!(!idx.is_empty(), "expected the primary-key index row");
}
