//! v7.37.17 (17.6 siblings) — pg_sleep + WAL LSN probes +
//! commit-timestamp helpers.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn pg_sleep_returns_void() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_sleep(0.1)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT pg_sleep_for('1 second')"),
        spg_storage::Value::Null
    ));
}

#[test]
fn wal_lsn_probes_return_text_or_null() {
    // v7.37.17 (17.6 siblings) — WAL LSN scalar functions now
    // return text "0/0" (postgres_exporter compatibility); the
    // xact_replay_timestamp probe still returns NULL.
    let mut e = Engine::new();
    for f in &[
        "pg_current_wal_lsn()",
        "pg_current_wal_flush_lsn()",
        "pg_current_wal_insert_lsn()",
        "pg_last_wal_receive_lsn()",
        "pg_last_wal_replay_lsn()",
    ] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "0/0"),
            other => panic!("SELECT {f}: expected Text(\"0/0\"), got {other:?}"),
        }
    }
    // Replay timestamp still NULL until v7.38 wall-clock plumbing.
    assert!(matches!(
        first(&mut e, "SELECT pg_last_xact_replay_timestamp()"),
        spg_storage::Value::Null
    ));
}

#[test]
fn xact_commit_timestamp_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_xact_commit_timestamp(1)"),
        spg_storage::Value::Null
    ));
}
