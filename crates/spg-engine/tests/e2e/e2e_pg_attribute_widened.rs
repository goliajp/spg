//! v7.37.24 (24.8b) — widened pg_attribute shape (16 PG-canonical
//! columns instead of the original 5). Tools introspecting column
//! metadata (length, identity, default, dimensions) now see the
//! same shape PG would surface.

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
fn pg_attribute_emits_pg_canonical_columns() {
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_catalog.pg_attribute").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("expected Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "attrelid",
        "attname",
        "atttypid",
        "attlen",
        "attnum",
        "attnotnull",
        "atthasdef",
        "attidentity",
        "attgenerated",
        "attisdropped",
        "attislocal",
        "attndims",
        "atttypmod",
        "attstorage",
        "attalign",
        "attinhcount",
        "attcollation",
    ] {
        assert!(
            names.contains(&must),
            "pg_attribute missing column {must}, got {names:?}"
        );
    }
}

#[test]
fn pg_attribute_atttypid_matches_pg_oid_for_int_and_text() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (n INT NOT NULL, s TEXT, b BIGINT, ts TIMESTAMPTZ)")
        .unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_attribute");
    // Position: 0 attrelid, 1 attname, 2 atttypid, ...
    let by_name = |name: &str| {
        rs.iter()
            .find(|r| matches!(&r[1], Value::Text(s) if s.as_ref() == name))
            .unwrap_or_else(|| panic!("missing {name}"))
    };
    let n = by_name("n");
    assert!(matches!(n[2], Value::BigInt(23)), "INT atttypid=23"); // int4
    let s = by_name("s");
    assert!(matches!(s[2], Value::BigInt(25)), "TEXT atttypid=25");
    let b = by_name("b");
    assert!(matches!(b[2], Value::BigInt(20)), "BIGINT atttypid=20"); // int8
    let ts = by_name("ts");
    assert!(matches!(ts[2], Value::BigInt(1184)), "TIMESTAMPTZ atttypid=1184");
}

#[test]
fn pg_attribute_attnotnull_reflects_column_nullability() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (required INT NOT NULL, optional INT)")
        .unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_attribute");
    // Position 11 = attnotnull (Bool).
    let required = rs
        .iter()
        .find(|r| matches!(&r[1], Value::Text(s) if s.as_ref() == "required"))
        .unwrap();
    let optional = rs
        .iter()
        .find(|r| matches!(&r[1], Value::Text(s) if s.as_ref() == "optional"))
        .unwrap();
    assert!(matches!(required[11], Value::Bool(true)));
    assert!(matches!(optional[11], Value::Bool(false)));
}

#[test]
fn pg_attribute_atthasdef_reflects_default_clause() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, status TEXT DEFAULT 'pending')")
        .unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_attribute");
    // Position 12 = atthasdef.
    let id = rs
        .iter()
        .find(|r| matches!(&r[1], Value::Text(s) if s.as_ref() == "id"))
        .unwrap();
    let status = rs
        .iter()
        .find(|r| matches!(&r[1], Value::Text(s) if s.as_ref() == "status"))
        .unwrap();
    assert!(matches!(id[12], Value::Bool(false)), "id has no DEFAULT");
    assert!(
        matches!(status[12], Value::Bool(true)),
        "status has DEFAULT"
    );
}

#[test]
fn pg_attribute_attlen_matches_pg_widths() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a SMALLINT, b INT, c BIGINT, d BOOLEAN, e TEXT)")
        .unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_attribute");
    // Position 4 = attlen (SmallInt).
    let by_name = |name: &str| {
        rs.iter()
            .find(|r| matches!(&r[1], Value::Text(s) if s.as_ref() == name))
            .unwrap()
    };
    assert!(matches!(by_name("a")[4], Value::SmallInt(2)));
    assert!(matches!(by_name("b")[4], Value::SmallInt(4)));
    assert!(matches!(by_name("c")[4], Value::SmallInt(8)));
    assert!(matches!(by_name("d")[4], Value::SmallInt(1)));
    assert!(matches!(by_name("e")[4], Value::SmallInt(-1)));
}
