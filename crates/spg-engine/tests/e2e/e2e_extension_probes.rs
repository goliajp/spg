//! v7.37.17 (17.6 siblings) — pg_available_extensions +
//! extension_version + pg_load_extension probes.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn extension_introspection_returns_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_available_extensions()",
        "pg_available_extension_versions()",
        "pg_extension_update_paths('pgcrypto')",
        "pg_extension_config_dump('pg_class', 'WHERE oid > 100')",
        "pg_load_extension('pgcrypto')",
        "pg_extension_check_version('pgcrypto')",
        "extension_version('pgcrypto')",
        "pg_visible_in_snapshot_txid(1)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
