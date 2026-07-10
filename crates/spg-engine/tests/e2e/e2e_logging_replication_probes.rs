//! v7.37.17 (17.6 siblings) — pg_current_logfile + replication-origin
//! + replication-slot admin probes.

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
fn pg_current_logfile_returns_empty_text() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_current_logfile()") {
        spg_storage::Value::Text(s) => assert!(s.as_ref().is_empty()),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn config_file_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_hba_file_rules()",
        "pg_ident_file_mappings()",
        "pg_config()",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

#[test]
fn replication_origin_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_replication_origin_advance('n', '0/0')",
        "pg_replication_origin_create('n')",
        "pg_replication_origin_drop('n')",
        "pg_replication_origin_oid('n')",
        "pg_replication_origin_progress('n', true)",
        "pg_replication_origin_session_is_setup()",
        "pg_replication_origin_session_progress(true)",
        "pg_replication_origin_session_reset()",
        "pg_replication_origin_session_setup('n')",
        "pg_replication_origin_xact_reset()",
        "pg_replication_origin_xact_setup('0/0', '2020-01-01'::timestamp)",
        "pg_show_replication_origin_status()",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

#[test]
fn replication_slot_admin_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_create_physical_replication_slot('slot1')",
        "pg_create_logical_replication_slot('slot2', 'pgoutput')",
        "pg_copy_physical_replication_slot('a', 'b')",
        "pg_copy_logical_replication_slot('a', 'b', true)",
        "pg_drop_replication_slot('slot1')",
        "pg_replication_slot_advance('slot1', '0/0')",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
