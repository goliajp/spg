//! v7.37.17 (17.6 siblings) — pg_bytes_pretty (alias for
//! pg_size_pretty) + pg_object_size + pg_relation_size_pretty.

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
fn pg_bytes_pretty_matches_pg_size_pretty() {
    let mut e = Engine::new();
    for n in &[0i64, 1024, 10239, 10240, 10_485_760, 10_737_418_240] {
        let a = format!("SELECT pg_bytes_pretty({n}::bigint)");
        let b = format!("SELECT pg_size_pretty({n}::bigint)");
        assert_eq!(text(&first(&mut e, &a)), text(&first(&mut e, &b)));
    }
}

#[test]
fn pg_object_size_returns_zero() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_object_size('users')") {
        spg_storage::Value::BigInt(0) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn pg_bytes_pretty_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_bytes_pretty(NULL::bigint)"),
        spg_storage::Value::Null
    ));
}
