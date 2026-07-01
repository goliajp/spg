//! v7.37.17 (17.6 siblings) — pg_stat_get_function_* +
//! pg_stat_get_slru_* per-function + SLRU probes.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn function_stat_probes_return_zero() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_get_function_calls(1)",
        "pg_stat_get_function_total_time(1)",
        "pg_stat_get_function_self_time(1)",
        "pg_stat_get_xact_function_calls(1)",
        "pg_stat_get_xact_function_total_time(1)",
        "pg_stat_get_xact_function_self_time(1)",
    ] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::BigInt(0) => {}
            other => panic!("SELECT {f}: got {other:?}"),
        }
    }
}

#[test]
fn slru_stat_probes_return_zero() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_get_slru_blks_zeroed('SUBTRANS')",
        "pg_stat_get_slru_blks_hit('SUBTRANS')",
        "pg_stat_get_slru_blks_read('SUBTRANS')",
        "pg_stat_get_slru_blks_written('SUBTRANS')",
        "pg_stat_get_slru_blks_exists('SUBTRANS')",
        "pg_stat_get_slru_flushes('SUBTRANS')",
        "pg_stat_get_slru_truncates('SUBTRANS')",
        "pg_stat_get_slru_stat_reset_time('SUBTRANS')",
    ] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::BigInt(0) => {}
            other => panic!("SELECT {f}: got {other:?}"),
        }
    }
}
