//! Scalar-array ::text[] casts + TIME ± INTERVAL wrap-around.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn int_array_to_text_array() {
    let mut e = Engine::new();
    let spg_storage::Value::TextArray(items) = one(&mut e, "SELECT ARRAY[1,2,3]::text[]") else {
        panic!("expected TextArray");
    };
    let got: Vec<Option<String>> = items;
    assert_eq!(
        got,
        vec![
            Some(String::from("1")),
            Some(String::from("2")),
            Some(String::from("3")),
        ]
    );
    // bigint array too.
    let spg_storage::Value::TextArray(items) =
        one(&mut e, "SELECT ARRAY[10,20]::bigint[]::text[]")
    else {
        panic!("expected TextArray");
    };
    assert_eq!(items, vec![Some(String::from("10")), Some(String::from("20"))]);
}

#[test]
fn time_plus_interval_wraps() {
    let mut e = Engine::new();
    let time = |v: spg_storage::Value<'_>| match v {
        spg_storage::Value::Time(t) => t,
        other => panic!("expected Time, got {other:?}"),
    };
    // 10:20:30 + 1h = 11:20:30.
    assert_eq!(
        time(one(&mut e, "SELECT '10:20:30'::time + '1 hour'::interval")),
        (11 * 3600 + 20 * 60 + 30) * 1_000_000
    );
    // 23:30 + 1h wraps to 00:30.
    assert_eq!(
        time(one(&mut e, "SELECT '23:30:00'::time + '1 hour'::interval")),
        30 * 60 * 1_000_000
    );
    // 10:20:30 - 11h wraps back to 23:20:30.
    assert_eq!(
        time(one(&mut e, "SELECT '10:20:30'::time - '11 hours'::interval")),
        (23 * 3600 + 20 * 60 + 30) * 1_000_000
    );
}
