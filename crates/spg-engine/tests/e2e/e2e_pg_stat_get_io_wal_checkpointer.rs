//! v7.37.17 (17.6 siblings) — pg_stat_get_io + _wal + backend
//! + checkpointer + recovery-prefetch probes.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn setof_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_get_wal()",
        "pg_stat_get_io()",
        "pg_stat_get_activity_start_time(1)",
        "pg_stat_get_backend_query_start(1)",
        "pg_stat_get_backend_leader_pid(1)",
        "pg_stat_get_backend_pid_by_activity_start('2020-01-01')",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

#[test]
fn recovery_prefetch_probes_return_zero() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_get_recovery_prefetch()",
        "pg_stat_get_recovery_prefetch_reset_time()",
    ] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::BigInt(0) => {}
            other => panic!("SELECT {f}: got {other:?}"),
        }
    }
}

#[test]
fn checkpointer_probes_return_zero() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_get_checkpointer_num_timed()",
        "pg_stat_get_checkpointer_num_requested()",
        "pg_stat_get_checkpointer_restartpoints_timed()",
        "pg_stat_get_checkpointer_restartpoints_requested()",
        "pg_stat_get_checkpointer_restartpoints_performed()",
        "pg_stat_get_checkpointer_write_time()",
        "pg_stat_get_checkpointer_sync_time()",
        "pg_stat_get_checkpointer_buffers_written()",
        "pg_stat_get_checkpointer_stat_reset_time()",
    ] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::BigInt(0) => {}
            other => panic!("SELECT {f}: got {other:?}"),
        }
    }
}
