//! ROWS FROM ( srf(), srf() ) — SQL-standard explicit
//! parallel-zip syntax over the multi-arg unnest channel.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter().map(|row| row.values.clone()).collect()
}

fn as_i64(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn zips_two_unnests() {
    let mut e = Engine::new();
    let got = rows(
        &mut e,
        "SELECT * FROM ROWS FROM (unnest(ARRAY[1, 2, 3]), \
         unnest(ARRAY['a','b'])) AS t(n, s)",
    );
    assert_eq!(got.len(), 3);
    assert_eq!(as_i64(&got[0][0]), 1);
    assert_eq!(text(&got[0][1]), "a");
    assert!(matches!(&got[2][1], spg_storage::Value::Null));
}

#[test]
fn mixes_srf_families_with_ordinality() {
    let mut e = Engine::new();
    // string_to_table rides its scalar array form inside ROWS FROM.
    let got = rows(
        &mut e,
        "SELECT s, k, ordinality FROM ROWS FROM (\
         string_to_table('x,y', ','), \
         jsonb_object_keys('{\"a\":1}')) WITH ORDINALITY AS t(s, k)",
    );
    assert_eq!(got.len(), 2);
    assert_eq!(text(&got[0][0]), "x");
    assert_eq!(text(&got[0][1]), "a");
    assert_eq!(as_i64(&got[1][2]), 2);
    assert!(matches!(&got[1][1], spg_storage::Value::Null));
}

#[test]
fn single_entry_and_unsupported_error() {
    let mut e = Engine::new();
    // Single entry rides the plain unnest channel.
    let got = rows(&mut e, "SELECT * FROM ROWS FROM (unnest(ARRAY[7])) AS t(v)");
    assert_eq!(got.len(), 1);
    assert_eq!(as_i64(&got[0][0]), 7);
    // generate_series has no scalar array form — honest error.
    let err = e
        .execute("SELECT * FROM ROWS FROM (generate_series(1, 3)) AS t(v)")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("generate_series"), "unexpected error: {msg}");
}
