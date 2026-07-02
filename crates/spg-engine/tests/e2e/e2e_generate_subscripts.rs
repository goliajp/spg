//! v7.37.17 (17.6 siblings) — generate_subscripts(arr, dim
//! [, reverse]), scalar IntArray surface + FROM-position SRF form.

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

fn ints(got: &[Vec<spg_storage::Value<'static>>]) -> Vec<i64> {
    got.iter()
        .map(|r| match &r[0] {
            spg_storage::Value::Int(n) => i64::from(*n),
            spg_storage::Value::BigInt(n) => *n,
            other => panic!("expected Int, got {other:?}"),
        })
        .collect()
}

#[test]
fn from_generate_subscripts_rows() {
    let mut e = Engine::new();
    // PG doc vector: generate_subscripts('{NULL,1,NULL,2}'::int[], 1)
    // → 1 / 2 / 3 / 4 (subscripts, NULL items still count).
    let got = rows(
        &mut e,
        "SELECT s FROM generate_subscripts(ARRAY[10, 20, 30, 40], 1) AS s",
    );
    assert_eq!(ints(&got), [1, 2, 3, 4]);
}

#[test]
fn reverse_form_and_natural_column() {
    let mut e = Engine::new();
    let got = rows(
        &mut e,
        "SELECT generate_subscripts \
         FROM generate_subscripts(ARRAY['a', 'b', 'c'], 1, true)",
    );
    assert_eq!(ints(&got), [3, 2, 1]);
}

#[test]
fn missing_dimension_yields_no_rows() {
    let mut e = Engine::new();
    let got = rows(
        &mut e,
        "SELECT s FROM generate_subscripts(ARRAY[1, 2], 2) AS s",
    );
    assert!(got.is_empty());
}
