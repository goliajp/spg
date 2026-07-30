//! v7.37.17 (17.6 siblings) — logical decoding + collation
//! versioning + binary-upgrade probes.

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
fn logical_slot_consumers_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_logical_slot_get_changes('s', NULL, NULL)",
        "pg_logical_slot_peek_changes('s', NULL, NULL)",
        "pg_logical_slot_get_binary_changes('s', NULL, NULL)",
        "pg_logical_slot_peek_binary_changes('s', NULL, NULL)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

#[test]
fn logical_emit_message_returns_lsn() {
    let mut e = Engine::new();
    match first(
        &mut e,
        "SELECT pg_logical_emit_message(true, 'prefix', 'content')",
    ) {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "0/0"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn collation_actual_version_matches_unicode() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_collation_actual_version(100)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "15.0"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT pg_database_collation_actual_version(1)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "15.0"),
        other => panic!("got {other:?}"),
    }
    assert!(matches!(
        first(&mut e, "SELECT pg_collation_actual_version(NULL::int)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn import_system_collations_zero() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_import_system_collations('pg_catalog')"),
        spg_storage::Value::Int(0)
    ));
}

#[test]
fn trigger_fn_names_and_binary_upgrade_return_null() {
    let mut e = Engine::new();
    // v7.39 (round 637) — the two trigger functions REFUSE a scalar call
    // now, as PG does ("not fired by trigger manager" / "must be called as
    // trigger"). The binary-upgrade setters keep answering NULL: PG has no
    // such functions at all, so there is nothing to match, and the NULL is
    // what keeps a pg_upgrade-generated dump moving.
    for f in &[
        "suppress_redundant_updates_trigger()",
        "tsvector_update_trigger()",
    ] {
        let sql = format!("SELECT {f}");
        let m = e.execute(&sql).expect_err("PG refuses a scalar call").to_string();
        assert!(
            m.contains("trigger"),
            "SELECT {f}: wanted a trigger-manager rejection, said {m:?}"
        );
    }
    for f in &[
        "pg_nextoid(1259, 1, 2662)",
        "binary_upgrade_set_next_pg_type_oid(16384)",
        "binary_upgrade_create_empty_extension('x', 'public', false, '1.0', NULL, NULL, NULL)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
