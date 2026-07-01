//! v7.37.17 (17.6 siblings) — pg_lock_status + pg_stat_get_progress_*.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn lock_and_progress_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_lock_status()",
        "pg_stat_get_progress_command(1)",
        "pg_stat_get_progress_relid(1)",
        "pg_stat_get_progress_datid(1)",
        "pg_stat_get_progress_pid(1)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
