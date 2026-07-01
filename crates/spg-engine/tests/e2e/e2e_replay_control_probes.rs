//! v7.37.17 (17.6 siblings) — WAL replay control + wait-event probes.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn wal_replay_control_returns_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_wal_replay_pause()",
        "pg_wal_replay_resume()",
        "pg_xlog_replay_pause()",
        "pg_xlog_replay_resume()",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

#[test]
fn pg_get_wal_replay_pause_state_returns_text() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT pg_get_wal_replay_pause_state()")),
        "not paused"
    );
}

#[test]
fn wait_event_probes_return_text() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT pg_get_wait_event_type(1)")),
        "Client"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT pg_get_wait_event_name(1)")),
        "Idle"
    );
}
