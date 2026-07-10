//! v7.37.17 (17.6 siblings) — pg_stat_statements support fns.

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
fn pg_stat_statements_support_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_statements_info()",
        "pg_stat_statements_reset()",
        "pg_stat_statements_reset_shared_memory_stats()",
        "pg_stat_statements()",
        "pg_get_shmem_allocations()",
        "pg_config_env()",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
