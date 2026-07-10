//! v7.37.17 (17.6 siblings) — pg_lsn operator support functions.

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

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn pg_lsn_larger_picks_max() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT pg_lsn_larger('0/1000', '0/2000')")),
        "0/2000"
    );
    // Cross the 32-bit boundary: 1/0 > 0/FFFFFFFF.
    assert_eq!(
        text(&first(&mut e, "SELECT pg_lsn_larger('1/0', '0/FFFFFFFF')")),
        "1/0"
    );
}

#[test]
fn pg_lsn_smaller_picks_min() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT pg_lsn_smaller('0/1000', '0/2000')")),
        "0/1000"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT pg_lsn_smaller('1/0', '0/FFFFFFFF')")),
        "0/FFFFFFFF"
    );
}

#[test]
fn pg_lsn_hash_deterministic() {
    let mut e = Engine::new();
    let a = first(&mut e, "SELECT pg_lsn_hash('0/1000')");
    let b = first(&mut e, "SELECT pg_lsn_hash('0/1000')");
    match (&a, &b) {
        (spg_storage::Value::Int(x), spg_storage::Value::Int(y)) => assert_eq!(x, y),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn pg_lsn_bad_input_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT pg_lsn_larger('bogus', '0/0')").is_err());
}

#[test]
fn pg_lsn_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_lsn_larger(NULL::text, '0/0')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT pg_lsn_hash(NULL::text)"),
        spg_storage::Value::Null
    ));
}
