//! v7.37.17 (17.6 siblings) — pg_backend_start_time /
//! pg_postmaster_start_time / pg_conf_load_time + backend probes.

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
fn start_time_probes_return_timestamp() {
    let mut e = Engine::new();
    for f in &[
        "pg_backend_start_time()",
        "pg_postmaster_start_time()",
        "pg_conf_load_time()",
        "pg_stat_get_backend_start(1)",
    ] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::Timestamp(_) => {}
            other => panic!("SELECT {f}: got {other:?}"),
        }
    }
}

#[test]
fn backend_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_get_backend_activity_start(1)",
        "pg_stat_get_backend_client_addr(1)",
        "pg_stat_get_backend_client_port(1)",
        "pg_stat_get_backend_dbid(1)",
        "pg_stat_get_backend_pid(1)",
        "pg_stat_get_backend_userid(1)",
        "pg_stat_get_backend_wait_event(1)",
        "pg_stat_get_backend_wait_event_type(1)",
        "pg_stat_get_backend_xact_start(1)",
        "pg_stat_get_backend_subxact(1)",
        "pg_stat_get_backend_idset()",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
