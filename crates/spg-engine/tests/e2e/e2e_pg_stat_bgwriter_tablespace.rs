//! v7.37.22 (22.16) + v7.37.23 (23.6-b) — pg_stat_bgwriter +
//! pg_tablespace views.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

#[test]
fn pg_stat_bgwriter_returns_single_row_with_pg_columns() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_catalog.pg_stat_bgwriter")
        .unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "checkpoints_timed",
        "checkpoints_req",
        "checkpoint_write_time",
        "checkpoint_sync_time",
        "buffers_checkpoint",
        "buffers_clean",
        "maxwritten_clean",
        "buffers_backend",
        "buffers_backend_fsync",
        "buffers_alloc",
        "stats_reset",
    ] {
        assert!(
            names.contains(&must),
            "pg_stat_bgwriter missing {must}: {names:?}"
        );
    }
    assert_eq!(rows.len(), 1);
}

#[test]
fn pg_tablespace_returns_pg_default_and_pg_global() {
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_catalog.pg_tablespace").unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in ["oid", "spcname", "spcowner", "spcacl", "spcoptions"] {
        assert!(
            names.contains(&must),
            "pg_tablespace missing {must}: {names:?}"
        );
    }
    assert_eq!(rows.len(), 2);
    let spcnames: Vec<String> = rows
        .iter()
        .filter_map(|r| {
            if let Value::Text(s) = &r.values[1] {
                Some(s.to_string())
            } else {
                None
            }
        })
        .collect();
    assert!(spcnames.contains(&"pg_default".to_string()));
    assert!(spcnames.contains(&"pg_global".to_string()));
}

#[test]
fn pg_tablespace_carries_pg_canonical_oids() {
    // PG hard-codes 1663 = pg_default, 1664 = pg_global.
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_catalog.pg_tablespace").unwrap();
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
    assert!(oids.contains(&1663), "pg_default OID 1663 missing");
    assert!(oids.contains(&1664), "pg_global OID 1664 missing");
}
