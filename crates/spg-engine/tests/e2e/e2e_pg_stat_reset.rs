//! v7.37.17 (17.6 siblings) — pg_stat_reset family — monitoring
//! dashboards call these on schedule; SPG returns void until
//! real per-view reset lands with v7.38 observability epic.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn pg_stat_reset_variants_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_reset()",
        "pg_stat_reset_shared('archiver')",
        "pg_stat_reset_single_table_counters(1)",
        "pg_stat_reset_single_function_counters(1)",
        "pg_stat_reset_slru('SUBTRANS')",
        "pg_stat_reset_replication_slot('slot1')",
        "pg_stat_reset_subscription_stats(1)",
        "pg_stat_clear_snapshot()",
        "pg_stat_force_next_flush()",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f}: should be NULL"
        );
    }
}
