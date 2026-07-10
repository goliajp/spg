//! v7.37.17 (17.6 siblings) — pg_stat_get_snapshot_timestamp
//! returns a real timestamp for stats-freshness monitoring.

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
fn snapshot_timestamp_returns_timestamp() {
    let mut e = Engine::new();
    for f in &[
        "pg_stat_get_snapshot_timestamp()",
        "pg_stat_get_stat_snapshot_timestamp()",
    ] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::Timestamp(_) => {}
            other => panic!("SELECT {f}: expected Timestamp, got {other:?}"),
        }
    }
}
