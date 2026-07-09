//! v7.37.17 (17.6 siblings) — transaction ID + status probes.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn txid_current_returns_bigint() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT txid_current()") {
        spg_storage::Value::BigInt(_) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn pg_current_xact_id_returns_bigint() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_current_xact_id()") {
        spg_storage::Value::BigInt(_) => {}
        other => panic!("got {other:?}"),
    }
    // v7.38 (T24) — `_if_assigned` is NULL until the transaction has an id; a
    // read-only autocommit statement has none, as in PG. It used to return the
    // constant-1 stub.
    match first(&mut e, "SELECT pg_current_xact_id_if_assigned()") {
        spg_storage::Value::Null => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn snapshot_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "txid_current_snapshot()",
        "pg_current_snapshot()",
        "pg_snapshot_xmin(pg_current_snapshot())",
        "pg_snapshot_xmax(pg_current_snapshot())",
    ] {
        let sql = format!("SELECT {f}");
        assert!(matches!(first(&mut e, &sql), spg_storage::Value::Null));
    }
}

#[test]
fn xact_status_returns_committed() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT txid_status(1)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "committed"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT pg_xact_status(1)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "committed"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn pg_notification_queue_usage_returns_zero() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_notification_queue_usage()") {
        spg_storage::Value::Float(f) => assert_eq!(f, 0.0),
        other => panic!("got {other:?}"),
    }
}
