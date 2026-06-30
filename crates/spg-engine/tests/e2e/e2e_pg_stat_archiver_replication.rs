//! v7.37.22 (22.17) + v7.37.21 (21.13-d) — pg_stat_archiver +
//! pg_stat_replication views. Both shape-stable; pg_stat_archiver
//! single row of zeros (SPG uses in-process WAL pubsub, no
//! archive_command), pg_stat_replication empty until 21.x wires
//! sender state.

use spg_engine::{Engine, QueryResult};

#[test]
fn pg_stat_archiver_returns_single_zero_row_with_pg_columns() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_catalog.pg_stat_archiver")
        .unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "archived_count",
        "last_archived_wal",
        "last_archived_time",
        "failed_count",
        "last_failed_wal",
        "last_failed_time",
        "stats_reset",
    ] {
        assert!(
            names.contains(&must),
            "pg_stat_archiver missing {must}: {names:?}"
        );
    }
    assert_eq!(rows.len(), 1);
}

#[test]
fn pg_stat_replication_returns_empty_with_pg_columns() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_catalog.pg_stat_replication")
        .unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "pid",
        "usename",
        "application_name",
        "client_addr",
        "state",
        "sent_lsn",
        "write_lsn",
        "flush_lsn",
        "replay_lsn",
        "sync_state",
        "reply_time",
    ] {
        assert!(
            names.contains(&must),
            "pg_stat_replication missing {must}: {names:?}"
        );
    }
    assert!(rows.is_empty());
}
