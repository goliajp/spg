//! v7.37.21 (21.13) — `pg_catalog.pg_replication_slots` view.
//! Shape-stable empty for now; SPG's logical replication via
//! MAGIC_SUB doesn't yet persist slot state across engine
//! restarts. Monitoring dashboards keep parsing.

use spg_engine::{Engine, QueryResult};

#[test]
fn pg_replication_slots_returns_pg_canonical_columns() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_catalog.pg_replication_slots")
        .unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("expected Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "slot_name",
        "plugin",
        "slot_type",
        "datoid",
        "database",
        "temporary",
        "active",
        "active_pid",
        "xmin",
        "catalog_xmin",
        "restart_lsn",
        "confirmed_flush_lsn",
        "wal_status",
        "safe_wal_size",
    ] {
        assert!(
            names.contains(&must),
            "pg_replication_slots missing column {must}, got {names:?}"
        );
    }
}

#[test]
fn pg_replication_slots_is_empty_until_persistent_slots_land() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_catalog.pg_replication_slots")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    assert!(rows.is_empty());
}
