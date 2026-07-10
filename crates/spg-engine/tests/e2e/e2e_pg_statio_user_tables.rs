//! v7.37.22 (22.2) — `pg_statio_user_tables` per-relation I/O
//! stats. PG-compatible shape so monitoring tools that pull the
//! standard pg_stat surfaces (pgwatch / pganalyze / Datadog
//! Postgres integration) don't break against SPG.

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
fn pg_statio_user_tables_lists_every_user_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE foo (id INT)").unwrap();
    e.execute("CREATE TABLE bar (id INT)").unwrap();
    e.execute("INSERT INTO foo VALUES (1), (2), (3)").unwrap();
    e.execute("INSERT INTO bar VALUES (1)").unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_statio_user_tables");
    // Two user tables.
    assert!(
        rs.len() == 2,
        "expected 2 rows, got {} ({:?})",
        rs.len(),
        rs
    );
    // Column ordering: relid, schemaname, relname, heap_blks_read,
    // heap_blks_hit, idx_blks_read, idx_blks_hit, toast_blks_read,
    // toast_blks_hit, tidx_blks_read, tidx_blks_hit
    let foo = rs
        .iter()
        .find(|r| matches!(&r[2], Value::Text(s) if s.as_ref() == "foo"))
        .expect("missing foo row");
    assert!(matches!(foo[1], Value::Text(ref s) if s.as_ref() == "public"));
    assert!(matches!(foo[4], Value::BigInt(3)), "foo heap_blks_hit");
    let bar = rs
        .iter()
        .find(|r| matches!(&r[2], Value::Text(s) if s.as_ref() == "bar"))
        .expect("missing bar row");
    assert!(matches!(bar[4], Value::BigInt(1)), "bar heap_blks_hit");
}

#[test]
fn pg_statio_user_tables_empty_when_no_user_tables() {
    let mut e = Engine::new();
    let rs = rows(&mut e, "SELECT * FROM pg_statio_user_tables");
    assert!(rs.is_empty(), "expected 0 rows, got {rs:?}");
}
