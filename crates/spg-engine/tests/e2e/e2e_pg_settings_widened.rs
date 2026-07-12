//! v7.37.17 (17.6 siblings) — pg_settings default row set widens to
//! match the SHOW-<param> PG-default fallbacks so both surfaces
//! return the same value for each GUC.

use spg_engine::{Engine, QueryResult};

#[test]
fn pg_settings_has_all_widened_defaults() {
    let mut e = Engine::new();
    let r = e.execute("SELECT name, setting FROM pg_settings").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("Rows");
    };
    let names: alloc::vec::Vec<String> = rows
        .iter()
        .filter_map(|r| match &r.values[0] {
            spg_storage::Value::Text(s) => Some(s.to_string()),
            _ => None,
        })
        .collect();
    for expected in &[
        "lock_timeout",
        "idle_in_transaction_session_timeout",
        "work_mem",
        "shared_buffers",
        "effective_cache_size",
        "row_security",
        "default_table_access_method",
        "search_path",
        "IntervalStyle",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "expected {expected} in pg_settings, names={names:?}"
        );
    }
}

#[test]
fn pg_settings_lock_timeout_is_bare_ms_with_unit() {
    let mut e = Engine::new();
    // v7.39 (GUC knife 2) — PG18 reports ms-unit time GUCs in
    // pg_settings as the bare millisecond count with unit = 'ms'
    // ("250ms" -> 250 | ms), NOT the SHOW rendering. Verified against
    // the live oracle.
    e.execute("SET lock_timeout = '250ms'").unwrap();
    let r = e
        .execute("SELECT setting, unit FROM pg_settings WHERE name = 'lock_timeout'")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("Rows");
    };
    assert_eq!(rows.len(), 1);
    match (&rows[0].values[0], &rows[0].values[1]) {
        (spg_storage::Value::Text(s), spg_storage::Value::Text(u)) => {
            assert_eq!(s.as_ref(), "250");
            assert_eq!(u.as_ref(), "ms");
        }
        other => panic!("got {other:?}"),
    }
}

extern crate alloc;
