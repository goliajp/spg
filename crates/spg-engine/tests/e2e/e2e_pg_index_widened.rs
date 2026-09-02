//! v7.37.24 (24.8b-2) — widened pg_index shape (19 PG-canonical
//! columns instead of the original 5). Tools introspecting
//! indexes (pgAdmin's index explorer, ORM index metadata
//! lookups) now see the columns they query.

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
fn pg_index_emits_pg_canonical_columns() {
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_catalog.pg_index").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("expected Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "indexrelid",
        "indrelid",
        "indnatts",
        "indnkeyatts",
        "indisunique",
        "indisprimary",
        "indisvalid",
        "indisready",
        "indislive",
        "indkey",
        "indcollation",
        "indclass",
        "indoption",
    ] {
        assert!(
            names.contains(&must),
            "pg_index missing column {must}, got {names:?}"
        );
    }
}

#[test]
fn pg_index_indrelid_joins_with_pg_class_oid() {
    // The whole point of indrelid being a real OID (not a synthetic
    // row index) is so the standard JOIN `pg_index JOIN pg_class
    // ON indrelid = oid` works. Verify by reading both views and
    // matching by OID.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
    e.execute("CREATE INDEX ix_t_name ON t(name)").unwrap();
    let class_rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_class");
    let t_oid = class_rs
        .iter()
        .find(|r| matches!(&r[1], Value::Text(s) if s.as_ref() == "t"))
        .map(|r| match r[0] {
            Value::BigInt(oid) => oid,
            _ => panic!("oid wrong type"),
        })
        .expect("t in pg_class");
    let index_rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_index");
    let ix_row = index_rs
        .iter()
        .find(|r| matches!(r[1], Value::BigInt(oid) if oid == t_oid))
        .expect("index for t in pg_index");
    // Index has 1 indexed column → indnatts = indnkeyatts = 1.
    assert!(matches!(ix_row[2], Value::SmallInt(1)));
    assert!(matches!(ix_row[3], Value::SmallInt(1)));
}

#[test]
fn pg_index_indkey_carries_one_based_column_position() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT, b INT, c INT)").unwrap();
    // Index on c (3rd column = position 3 in PG; SPG's
    // column_position is 0-based and gets bumped).
    e.execute("CREATE INDEX ix_t_c ON t(c)").unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_index");
    let ix = rs.first().expect("one index row");
    // v7.39.11 — `indkey` is a real `int2vector` now, not the text
    // that happened to print the same. The claim this pin makes is
    // unchanged: the position is PG's 1-based attnum, where SPG stores
    // it 0-based.
    match &ix[15] {
        Value::Int2Vector(v) => assert_eq!(v.as_slice(), [3], "indkey should be `3`"),
        other => panic!("indkey wrong type: {other:?}"),
    }
    assert_eq!(
        spg_engine::eval::value_to_text(&ix[15]),
        "3",
        "and it still PRINTS the way PG prints an int2vector"
    );
}

#[test]
fn pg_index_indisunique_reflects_unique_index() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
    e.execute("CREATE UNIQUE INDEX ix_t_name ON t(name)")
        .unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_index");
    let ix = rs.first().expect("one index row");
    // Position 4 = indisunique.
    assert!(matches!(ix[4], Value::Bool(true)));
}
