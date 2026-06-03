//! v6.7.2 — per-table hot/cold byte budget via `ALTER TABLE`.

use spg_engine::{Engine, QueryResult};

#[test]
fn alter_table_set_hot_tier_bytes_round_trips() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("ALTER TABLE t SET hot_tier_bytes = 4096").unwrap();
    let table = eng.catalog().get("t").expect("table present");
    assert_eq!(table.schema().hot_tier_bytes, Some(4096));
}

#[test]
fn alter_table_overwrites_previous_setting() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("ALTER TABLE t SET hot_tier_bytes = 1000").unwrap();
    eng.execute("ALTER TABLE t SET hot_tier_bytes = 2000").unwrap();
    let table = eng.catalog().get("t").expect("table present");
    assert_eq!(table.schema().hot_tier_bytes, Some(2000));
}

#[test]
fn alter_table_unknown_table_errors() {
    let mut eng = Engine::new();
    let r = eng.execute("ALTER TABLE missing SET hot_tier_bytes = 1");
    assert!(r.is_err(), "ALTER TABLE on a non-existent table must error");
}

#[test]
fn alter_table_unknown_setting_errors_at_parse() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT)").unwrap();
    let r = eng.execute("ALTER TABLE t SET unknown_setting = 42");
    assert!(r.is_err(), "unknown setting must error");
}

#[test]
fn hot_tier_bytes_survives_snapshot_round_trip() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("CREATE TABLE u (id INT NOT NULL)").unwrap();
    eng.execute("ALTER TABLE t SET hot_tier_bytes = 12345").unwrap();
    // u has no override → should round-trip as None.
    let snapshot = eng.snapshot();
    // Reload via the catalog deserialiser by constructing a fresh
    // Engine and restoring.
    let restored_cat = spg_storage::Catalog::deserialize(&snapshot).expect("deserialize");
    assert_eq!(
        restored_cat.get("t").unwrap().schema().hot_tier_bytes,
        Some(12345)
    );
    assert_eq!(
        restored_cat.get("u").unwrap().schema().hot_tier_bytes,
        None
    );
}

#[test]
fn ddl_via_spg_table_ddl_does_not_include_hot_tier_bytes_yet() {
    // v6.7.2 ships persistence + freezer integration. Emitting the
    // SET clause through spg_table_ddl is OOS for v6.7.2 (would
    // require extending render_create_table to optionally append
    // an ALTER TABLE statement). Doc the gap for the v6.7.8
    // STABILITY rollup; verify the basic spg_table_ddl path still
    // works without it.
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("ALTER TABLE t SET hot_tier_bytes = 999").unwrap();
    let res = eng.execute("SELECT * FROM spg_table_ddl").unwrap();
    let rows = match res {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!("expected Rows"),
    };
    let ddl = rows
        .iter()
        .find_map(|r| {
            if let (
                spg_storage::Value::Text(name),
                spg_storage::Value::Text(ddl),
            ) = (&r.values[0], &r.values[1])
                && name == "t"
            {
                Some(ddl.clone())
            } else {
                None
            }
        })
        .expect("t row");
    // CREATE TABLE still emits; ALTER TABLE appendix is OOS for v6.7.2.
    assert!(ddl.starts_with("CREATE TABLE t"));
}
