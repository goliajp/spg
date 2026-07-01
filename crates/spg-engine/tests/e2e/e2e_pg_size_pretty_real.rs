//! v7.37.17 (17.6 siblings) — pg_size_pretty(bigint) — real
//! byte-to-human formatting matching PG's decision-point table.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn pg_size_pretty_boundaries() {
    let mut e = Engine::new();
    // Under 10240 (10 kB) — reports bytes.
    assert_eq!(text(&first(&mut e, "SELECT pg_size_pretty(0::bigint)")), "0 bytes");
    assert_eq!(text(&first(&mut e, "SELECT pg_size_pretty(1024::bigint)")), "1024 bytes");
    assert_eq!(text(&first(&mut e, "SELECT pg_size_pretty(10239::bigint)")), "10239 bytes");
    // ≥ 10 kB — reports kB.
    assert_eq!(text(&first(&mut e, "SELECT pg_size_pretty(10240::bigint)")), "10 kB");
    assert_eq!(text(&first(&mut e, "SELECT pg_size_pretty(102400::bigint)")), "100 kB");
    // ≥ 10 MB — reports MB. 10 MB = 10485760 bytes.
    assert_eq!(text(&first(&mut e, "SELECT pg_size_pretty(10485760::bigint)")), "10 MB");
    assert_eq!(text(&first(&mut e, "SELECT pg_size_pretty(1073741824::bigint)")), "1024 MB");
    // ≥ 10 GB.
    assert_eq!(text(&first(&mut e, "SELECT pg_size_pretty(10737418240::bigint)")), "10 GB");
    // ≥ 10 TB.
    assert_eq!(text(&first(&mut e, "SELECT pg_size_pretty(10995116277760::bigint)")), "10 TB");
}

#[test]
fn pg_size_pretty_roundtrips_with_pg_size_bytes() {
    let mut e = Engine::new();
    // pg_size_pretty(2 MB) → "2 MB"  ...   wait, 2*1024*1024 = 2097152 < 10 MB → "2048 kB".
    // Use 20 MB instead: 20 * 1024 * 1024 = 20971520.
    let pretty = text(&first(&mut e, "SELECT pg_size_pretty(20971520::bigint)"));
    assert_eq!(pretty, "20 MB");
    // Then reverse.
    let sql = format!("SELECT pg_size_bytes('{pretty}')");
    match first(&mut e, &sql) {
        spg_storage::Value::BigInt(20971520) => {}
        other => panic!("roundtrip: {other:?}"),
    }
}

#[test]
fn pg_size_pretty_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_size_pretty(NULL::bigint)"),
        spg_storage::Value::Null
    ));
}
