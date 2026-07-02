//! v7.37.17 (17.6 siblings) — pageinspect / pgstattuple /
//! pg_prewarm extension probes + tablespace locators.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn tablespace_location_empty_string() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_tablespace_location(1663)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), ""),
        other => panic!("got {other:?}"),
    }
    assert!(matches!(
        first(&mut e, "SELECT pg_tablespace_location(NULL::int)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn pageinspect_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "get_raw_page('t', 0)",
        "page_header('x'::bytea)",
        "heap_page_items('x'::bytea)",
        "bt_metap('idx')",
        "bt_page_stats('idx', 1)",
        "bt_page_items('idx', 1)",
        "brin_metapage_info('x'::bytea)",
        "gin_metapage_info('x'::bytea)",
        "hash_page_type('x'::bytea)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

#[test]
fn pgstattuple_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pgstattuple('t')",
        "pgstattuple_approx('t')",
        "pgstatindex('idx')",
        "pg_buffercache_summary()",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

#[test]
fn prewarm_and_relpages_return_zero() {
    let mut e = Engine::new();
    for f in &["pg_prewarm('t')", "pg_relpages('t')"] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::BigInt(0) => {}
            other => panic!("SELECT {f}: got {other:?}"),
        }
    }
}
