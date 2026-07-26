//! v7.37.17 (17.6 siblings) — index-property + schema-visibility
//! probes used by psql \d + monitoring exporters.

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
fn index_property_probes_return_true() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_index_has_property(1, 'returnable')") {
        spg_storage::Value::Bool(true) => {}
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT pg_indexam_has_property(1, 'clusterable')") {
        spg_storage::Value::Bool(true) => {}
        other => panic!("got {other:?}"),
    }
    match first(
        &mut e,
        "SELECT pg_index_column_has_property(1, 1, 'orderable')",
    ) {
        spg_storage::Value::Bool(true) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn visibility_probes_return_true() {
    let mut e = Engine::new();
    for f in &[
        "pg_type_is_visible(1)",
        "pg_table_is_visible(1)",
        "pg_function_is_visible(1)",
        "pg_operator_is_visible(1)",
        "pg_opclass_is_visible(1)",
        "pg_ts_config_is_visible(1)",
        "pg_ts_dict_is_visible(1)",
        "pg_ts_parser_is_visible(1)",
        "pg_ts_template_is_visible(1)",
    ] {
        // v7.39 (round 518) — these used to assert TRUE, which was SPG's
        // stub rather than PG's answer. PG looks the object up first, and
        // oid 1 names nothing — so NULL, not "visible". A tool asking about
        // a dropped relation used to be told it was still there.
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::Null => {}
            other => panic!("SELECT {f}: got {other:?}"),
        }
    }
}

#[test]
fn publication_and_stat_probes() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_relation_is_publishable(1)") {
        spg_storage::Value::Bool(true) => {}
        other => panic!("got {other:?}"),
    }
    for f in &[
        "pg_get_publication_tables('pub')",
        "pg_stat_get_activity(1)",
        "pg_stat_get_backend_activity(1)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
    // v7.37.17 (17.6 siblings) — pg_stat_get_snapshot_timestamp
    // upgraded from NULL to a real Timestamp for stats-freshness
    // monitoring dashboards. Verify shape only.
    match first(&mut e, "SELECT pg_stat_get_snapshot_timestamp()") {
        spg_storage::Value::Timestamp(_) => {}
        other => panic!("expected Timestamp, got {other:?}"),
    }
}
