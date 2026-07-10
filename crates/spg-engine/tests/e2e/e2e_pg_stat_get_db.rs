//! v7.37.17 (17.6 siblings) — pg_stat_get_db_* family probes.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn db_counter_probes_return_zero() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_get_db_xact_commit(1)",
        "pg_stat_get_db_xact_rollback(1)",
        "pg_stat_get_db_blocks_fetched(1)",
        "pg_stat_get_db_blocks_hit(1)",
        "pg_stat_get_db_tuples_returned(1)",
        "pg_stat_get_db_tuples_fetched(1)",
        "pg_stat_get_db_tuples_inserted(1)",
        "pg_stat_get_db_tuples_updated(1)",
        "pg_stat_get_db_tuples_deleted(1)",
        "pg_stat_get_db_conflict_all(1)",
        "pg_stat_get_db_deadlocks(1)",
        "pg_stat_get_db_checksum_failures(1)",
        "pg_stat_get_db_active_time(1)",
        "pg_stat_get_db_idle_in_transaction_time(1)",
        "pg_stat_get_db_session_time(1)",
        "pg_stat_get_db_sessions(1)",
        "pg_stat_get_db_temp_bytes(1)",
        "pg_stat_get_db_temp_files(1)",
        "pg_stat_get_db_numbackends(1)",
        "pg_stat_get_db_blk_read_time(1)",
        "pg_stat_get_db_blk_write_time(1)",
    ] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::BigInt(0) => {}
            other => panic!("SELECT {f}: expected BigInt(0), got {other:?}"),
        }
    }
}

#[test]
fn db_timestamp_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_get_db_stat_reset_time(1)",
        "pg_stat_get_db_checksum_last_failure(1)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
