//! v7.37.17 (17.6 siblings) — snapshot export/import + visibility
//! probes. Queue with v7.38 MVCC Phase C for real implementations.

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
fn snapshot_export_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_export_snapshot()",
        "pg_snapshot()",
        "pg_import_snapshot('00000001-1-1')",
        "pg_import_serialized_snapshot('00000001-1-1')",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

#[test]
fn pg_visible_in_snapshot_returns_true() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_visible_in_snapshot(1, NULL)") {
        spg_storage::Value::Bool(true) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn pg_last_xid_returns_bigint() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_last_xid()") {
        spg_storage::Value::BigInt(_) => {}
        other => panic!("got {other:?}"),
    }
}
