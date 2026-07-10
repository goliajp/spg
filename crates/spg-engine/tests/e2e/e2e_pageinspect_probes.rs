//! v7.37.17 (17.6 siblings) — pageinspect / pgstattuple /
//! pg_prewarm extension probes + tablespace locators.

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
fn prewarm_zero_and_relpages_real() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_prewarm('t')") {
        spg_storage::Value::BigInt(0) => {}
        other => panic!("pg_prewarm: got {other:?}"),
    }
    // pg_relpages is real now (hot_bytes / 8192, same meter as
    // pg_class.relpages): missing table → NULL, small table → ≥1
    // page once rows land.
    assert!(matches!(
        first(&mut e, "SELECT pg_relpages('t')"),
        spg_storage::Value::Null
    ));
    e.execute("CREATE TABLE rp (v TEXT)").unwrap();
    e.execute("INSERT INTO rp VALUES (repeat('z', 200))")
        .unwrap();
    match first(&mut e, "SELECT pg_relpages('rp')") {
        spg_storage::Value::BigInt(n) => assert!(n >= 1, "pages: {n}"),
        other => panic!("pg_relpages('rp'): got {other:?}"),
    }
}
