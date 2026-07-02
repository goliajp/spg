//! v7.37.17 (17.6 siblings) — top-level bare VALUES statement.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter()
        .map(|row| row.values.into_iter().collect())
        .collect()
}

fn as_i64(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn bare_values_rows() {
    let mut e = Engine::new();
    let got = rows(&mut e, "VALUES (1, 'one'), (2, 'two')");
    assert_eq!(got.len(), 2);
    assert_eq!(as_i64(&got[0][0]), 1);
    assert!(matches!(&got[1][1], spg_storage::Value::Text(s) if s == "two"));
}

#[test]
fn bare_values_order_by_and_limit() {
    let mut e = Engine::new();
    let got = rows(&mut e, "VALUES (3), (1), (2) ORDER BY column1 DESC LIMIT 2");
    assert_eq!(got.len(), 2);
    assert_eq!(as_i64(&got[0][0]), 3);
    assert_eq!(as_i64(&got[1][0]), 2);
}
