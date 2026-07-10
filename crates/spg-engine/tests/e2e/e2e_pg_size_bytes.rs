//! v7.37.17 (17.6 siblings) — pg_size_bytes(text) — human-readable
//! size → BigInt bytes.

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

fn as_bigint(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected BigInt, got {other:?}"),
    }
}

#[test]
fn pg_size_bytes_kb_mb_gb_tb() {
    let mut e = Engine::new();
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT pg_size_bytes('2 kB')")),
        2048
    );
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT pg_size_bytes('2 MB')")),
        2 * 1024 * 1024
    );
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT pg_size_bytes('2 GB')")),
        2i64 * 1024 * 1024 * 1024
    );
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT pg_size_bytes('1 TB')")),
        1024i64.pow(4)
    );
}

#[test]
fn pg_size_bytes_fractional() {
    let mut e = Engine::new();
    // 1.5 MB = 1572864 bytes.
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT pg_size_bytes('1.5 MB')")),
        1_572_864
    );
    // 0.5 GB = 536870912 bytes.
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT pg_size_bytes('0.5 GB')")),
        536_870_912
    );
}

#[test]
fn pg_size_bytes_no_unit_is_bytes() {
    let mut e = Engine::new();
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT pg_size_bytes('12345')")),
        12345
    );
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT pg_size_bytes('1024 bytes')")),
        1024
    );
}

#[test]
fn pg_size_bytes_case_insensitive() {
    let mut e = Engine::new();
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT pg_size_bytes('2 mb')")),
        2 * 1024 * 1024
    );
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT pg_size_bytes('2 KB')")),
        2048
    );
}

#[test]
fn pg_size_bytes_unknown_unit_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT pg_size_bytes('2 XB')").is_err());
    assert!(e.execute("SELECT pg_size_bytes('')").is_err());
}

#[test]
fn pg_size_bytes_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_size_bytes(NULL::text)"),
        spg_storage::Value::Null
    ));
}
