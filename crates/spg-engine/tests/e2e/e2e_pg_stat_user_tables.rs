//! v7.37.22 (22.14) — `pg_catalog.pg_stat_user_tables` view.

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
fn pg_stat_user_tables_emits_pg_canonical_columns() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_catalog.pg_stat_user_tables")
        .unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "relid",
        "schemaname",
        "relname",
        "seq_scan",
        "seq_tup_read",
        "idx_scan",
        "idx_tup_fetch",
        "n_tup_ins",
        "n_tup_upd",
        "n_tup_del",
        "n_live_tup",
        "n_dead_tup",
        "last_vacuum",
        "last_analyze",
    ] {
        assert!(
            names.contains(&must),
            "pg_stat_user_tables missing column {must}, got {names:?}"
        );
    }
}

#[test]
fn pg_stat_user_tables_lists_user_tables_with_live_tup_row_count() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE foo (id INT)").unwrap();
    e.execute("CREATE TABLE bar (id INT)").unwrap();
    e.execute("INSERT INTO foo VALUES (1), (2), (3)").unwrap();
    e.execute("INSERT INTO bar VALUES (1)").unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_stat_user_tables");
    assert_eq!(rs.len(), 2);
    // Position 10 = n_live_tup.
    let foo = rs
        .iter()
        .find(|r| matches!(&r[2], Value::Text(s) if s.as_ref() == "foo"))
        .unwrap();
    let bar = rs
        .iter()
        .find(|r| matches!(&r[2], Value::Text(s) if s.as_ref() == "bar"))
        .unwrap();
    assert!(matches!(foo[10], Value::BigInt(3)));
    assert!(matches!(bar[10], Value::BigInt(1)));
}

#[test]
fn pg_stat_user_tables_empty_when_no_user_tables() {
    let mut e = Engine::new();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_stat_user_tables");
    assert!(rs.is_empty(), "got {rs:?}");
}
