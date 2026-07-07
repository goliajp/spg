//! v7.38 (read01 P6.55) — CREATE TABLE ... WITH (storage params) is accepted
//! and ignored (SPG has no per-table reloptions), so a pg_dump that emits
//! WITH (fillfactor=…, autovacuum_enabled=…) restores cleanly.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) -> bool {
    matches!(e.execute(sql), Ok(QueryResult::CommandOk { .. } | QueryResult::Rows { .. }))
}

#[test]
fn create_table_with_reloptions_is_accepted() {
    let mut e = Engine::new();
    assert!(ok(
        &mut e,
        "CREATE TABLE t1 (id INT) WITH (fillfactor=70, autovacuum_enabled=false)"
    ));
    // The table is real and usable.
    e.execute("INSERT INTO t1 VALUES (1)").unwrap();
    assert!(matches!(
        e.execute("SELECT count(*) FROM t1").unwrap(),
        QueryResult::Rows { .. }
    ));
    assert!(ok(&mut e, "CREATE TABLE t2 (id INT) WITH (oids=false)"));
    // WITH reloptions coexists with PARTITION BY.
    assert!(ok(
        &mut e,
        "CREATE TABLE t3 (id INT, ts TIMESTAMPTZ) WITH (fillfactor=90) PARTITION BY RANGE (ts)"
    ));
}

#[test]
fn with_data_trailer_still_works() {
    // The reloptions consumer must not eat a `WITH NO DATA` CTAS trailer.
    let mut e = Engine::new();
    e.execute("CREATE TABLE src (x INT)").unwrap();
    e.execute("INSERT INTO src VALUES (1), (2)").unwrap();
    e.execute("CREATE TABLE dst AS SELECT x FROM src WITH NO DATA").unwrap();
    match e.execute("SELECT count(*) FROM dst").unwrap() {
        QueryResult::Rows { rows, .. } => assert_eq!(rows[0].values[0], spg_storage::Value::BigInt(0)),
        _ => panic!(),
    }
}
