//! v7.37.17 (17.6 siblings) — replication-slot + subscription
//! + progress-info + WAL sender/receiver scalar probes.

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
fn stat_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_get_replication_slot('slot1')",
        "pg_stat_get_subscription(1)",
        "pg_stat_get_subscription_stats(1)",
        "pg_stat_get_slru('SUBTRANS')",
        "pg_stat_get_progress_info('VACUUM')",
        "pg_stat_get_wal_senders()",
        "pg_stat_get_wal_receivers()",
        "pg_stat_get_client_addr(1)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
