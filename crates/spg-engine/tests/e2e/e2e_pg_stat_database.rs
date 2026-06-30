//! v7.37.22 (22.x-stat-db) — `pg_catalog.pg_stat_database` view.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

#[test]
fn pg_stat_database_emits_pg_canonical_columns() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_catalog.pg_stat_database")
        .unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "datid",
        "datname",
        "numbackends",
        "xact_commit",
        "xact_rollback",
        "blks_read",
        "blks_hit",
        "tup_returned",
        "tup_fetched",
        "tup_inserted",
        "tup_updated",
        "tup_deleted",
        "conflicts",
        "deadlocks",
        "temp_files",
        "temp_bytes",
        "blk_read_time",
        "blk_write_time",
    ] {
        assert!(
            names.contains(&must),
            "pg_stat_database missing column {must}, got {names:?}"
        );
    }
}

#[test]
fn pg_stat_database_returns_single_spg_row() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_catalog.pg_stat_database")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("Rows");
    };
    assert_eq!(rows.len(), 1, "expected single spg row, got {rows:?}");
    let row = &rows[0];
    // Position 0 = datid, 1 = datname.
    assert!(matches!(row.values[0], Value::BigInt(16384)));
    assert!(matches!(&row.values[1], Value::Text(s) if s.as_ref() == "spg"));
}
