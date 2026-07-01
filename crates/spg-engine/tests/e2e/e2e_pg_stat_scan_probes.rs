//! v7.37.17 (17.6 siblings) — pg_stat_get_last_scan +
//! seq/idx tuple + scan-position probes.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn scan_timestamp_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_get_last_scan(1)",
        "pg_stat_get_last_idx_scan(1)",
        "pg_stat_get_lastscan(1)",
        "pg_stat_get_lastidxscan(1)",
        "pg_stat_get_backend_role(1)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

#[test]
fn scan_counters_return_zero() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_get_seq_scan_pos(1)",
        "pg_stat_get_tid_scan_pos(1)",
        "pg_stat_get_seq_scan(1)",
        "pg_stat_get_idx_scan(1)",
        "pg_stat_get_seq_tup_read(1)",
        "pg_stat_get_idx_tup_read(1)",
        "pg_stat_get_idx_tup_fetch(1)",
    ] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::BigInt(0) => {}
            other => panic!("SELECT {f}: got {other:?}"),
        }
    }
}
