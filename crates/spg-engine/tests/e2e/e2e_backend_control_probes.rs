//! v7.37.17 (17.6 siblings) — pg_wait_for_backend_termination +
//! pg_isolation_test_session_is_blocked + pg_safe_snapshot_blocking_pids.

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
fn wait_for_backend_termination_returns_true() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_wait_for_backend_termination(1, 100)") {
        spg_storage::Value::Bool(true) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn isolation_probe_returns_false() {
    let mut e = Engine::new();
    match first(
        &mut e,
        "SELECT pg_isolation_test_session_is_blocked(1, ARRAY[1])",
    ) {
        spg_storage::Value::Bool(false) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn safe_snapshot_blocking_pids_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_safe_snapshot_blocking_pids(1)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn activity_started_at_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_stat_get_backend_activity_started_at(1)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn terminate_with_timeout_returns_true() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_terminate_backend_with_timeout(1, 100)") {
        spg_storage::Value::Bool(true) => {}
        other => panic!("got {other:?}"),
    }
}
