//! v7.37.17 (17.6 siblings) — pg_stat_get_bgwriter_* + wal_* +
//! archiver_* + per-table stat probes.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn bgwriter_and_wal_counters_return_zero() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_get_bgwriter_timed_checkpoints()",
        "pg_stat_get_bgwriter_requested_checkpoints()",
        "pg_stat_get_bgwriter_buf_written_checkpoints()",
        "pg_stat_get_bgwriter_buf_written_clean()",
        "pg_stat_get_bgwriter_maxwritten_clean()",
        "pg_stat_get_buf_written_backend()",
        "pg_stat_get_buf_fsync_backend()",
        "pg_stat_get_buf_alloc()",
        "pg_stat_get_checkpoint_write_time()",
        "pg_stat_get_checkpoint_sync_time()",
        "pg_stat_get_wal_records()",
        "pg_stat_get_wal_fpi()",
        "pg_stat_get_wal_bytes()",
        "pg_stat_get_wal_buffers_full()",
        "pg_stat_get_wal_write()",
        "pg_stat_get_wal_sync()",
        "pg_stat_get_wal_write_time()",
        "pg_stat_get_wal_sync_time()",
        "pg_stat_get_archiver_archived_count()",
        "pg_stat_get_archiver_failed_count()",
    ] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::BigInt(0) => {}
            other => panic!("SELECT {f}: expected BigInt(0), got {other:?}"),
        }
    }
}

#[test]
fn per_table_counters_return_zero() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_get_analyze_count(1)",
        "pg_stat_get_autoanalyze_count(1)",
        "pg_stat_get_vacuum_count(1)",
        "pg_stat_get_autovacuum_count(1)",
        "pg_stat_get_live_tuples(1)",
        "pg_stat_get_dead_tuples(1)",
        "pg_stat_get_mod_since_analyze(1)",
        "pg_stat_get_ins_since_vacuum(1)",
        "pg_stat_get_tuples_inserted(1)",
        "pg_stat_get_tuples_updated(1)",
        "pg_stat_get_tuples_deleted(1)",
        "pg_stat_get_tuples_hot_updated(1)",
        "pg_stat_get_tuples_newpage_updated(1)",
        "pg_stat_get_numscans(1)",
        "pg_stat_get_tuples_returned(1)",
        "pg_stat_get_tuples_fetched(1)",
        "pg_stat_get_blocks_fetched(1)",
        "pg_stat_get_blocks_hit(1)",
    ] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::BigInt(0) => {}
            other => panic!("SELECT {f}: expected BigInt(0), got {other:?}"),
        }
    }
}

#[test]
fn timestamp_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_get_bgwriter_stat_reset_time()",
        "pg_stat_get_archiver_last_archived_time()",
        "pg_stat_get_archiver_last_failed_time()",
        "pg_stat_get_archiver_stat_reset_time()",
        "pg_stat_get_last_analyze_time(1)",
        "pg_stat_get_last_autoanalyze_time(1)",
        "pg_stat_get_last_vacuum_time(1)",
        "pg_stat_get_last_autovacuum_time(1)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

#[test]
fn archiver_wal_text_probes_return_empty() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_get_archiver_last_archived_wal()",
        "pg_stat_get_archiver_last_failed_wal()",
    ] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::Text(s) => assert!(s.as_ref().is_empty()),
            other => panic!("SELECT {f}: expected empty Text, got {other:?}"),
        }
    }
}
