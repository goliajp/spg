//! v7.38 (read01 P3.14) — pg_stat_checkpointer / pg_stat_wal shell views.

use spg_engine::{Engine, QueryResult};

fn col_names(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
        QueryResult::Rows { columns, .. } => columns.iter().map(|c| c.name.clone()).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn checkpointer_and_wal_shell_views_have_pg_columns() {
    let mut e = Engine::new();
    assert_eq!(
        col_names(&mut e, "SELECT * FROM pg_catalog.pg_stat_checkpointer"),
        vec![
            "num_timed", "num_requested", "num_done", "restartpoints_timed",
            "restartpoints_req", "restartpoints_done", "write_time", "sync_time",
            "buffers_written", "slru_written", "stats_reset",
        ]
    );
    assert_eq!(
        col_names(&mut e, "SELECT * FROM pg_catalog.pg_stat_wal"),
        vec!["wal_records", "wal_fpi", "wal_bytes", "wal_buffers_full", "stats_reset"]
    );
    // Projection works too (via the meta-view pipeline).
    assert_eq!(
        col_names(&mut e, "SELECT wal_records, wal_bytes FROM pg_catalog.pg_stat_wal"),
        vec!["wal_records", "wal_bytes"]
    );
}

#[test]
fn slru_and_subscription_stats_shell_views() {
    // v7.38 (read01 P3.15) — pg_stat_slru / pg_stat_subscription_stats
    // shell views with PG columns (empty: SPG has no SLRU + doesn't track
    // subscription stats yet).
    let mut e = Engine::new();
    assert_eq!(
        col_names(&mut e, "SELECT * FROM pg_catalog.pg_stat_slru"),
        vec![
            "name", "blks_zeroed", "blks_hit", "blks_read", "blks_written",
            "blks_exists", "flushes", "truncates", "stats_reset",
        ]
    );
    assert_eq!(
        col_names(&mut e, "SELECT * FROM pg_catalog.pg_stat_subscription_stats"),
        vec![
            "subid", "subname", "apply_error_count", "sync_error_count",
            "confl_insert_exists", "confl_update_origin_differs", "confl_update_exists",
            "confl_update_missing", "confl_delete_origin_differs", "confl_delete_missing",
            "confl_multiple_unique_conflicts", "stats_reset",
        ]
    );
}
