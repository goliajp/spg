//! v7.37.17 (17.6 siblings) — PG 16+ any_value aggregate.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter()
        .map(|row| row.values.into_iter().map(|v| v.clone()).collect())
        .collect()
}

#[test]
fn any_value_picks_non_null() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE av (g INT, v TEXT)").unwrap();
    e.execute("INSERT INTO av VALUES (1, NULL), (1, 'first'), (1, 'second'), (2, 'only')")
        .unwrap();
    let got = rows(
        &mut e,
        "SELECT g, any_value(v) FROM av GROUP BY g ORDER BY g",
    );
    assert_eq!(got.len(), 2);
    // Group 1: some non-NULL member (SPG picks the first seen).
    match &got[0][1] {
        spg_storage::Value::Text(s) => {
            assert!(s.as_ref() == "first" || s.as_ref() == "second");
        }
        other => panic!("group 1: got {other:?}"),
    }
    match &got[1][1] {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "only"),
        other => panic!("group 2: got {other:?}"),
    }
}

#[test]
fn any_value_all_null_group_is_null() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE avn (g INT, v TEXT)").unwrap();
    e.execute("INSERT INTO avn VALUES (1, NULL), (1, NULL)")
        .unwrap();
    let got = rows(&mut e, "SELECT any_value(v) FROM avn GROUP BY g");
    assert_eq!(got.len(), 1);
    assert!(matches!(got[0][0], spg_storage::Value::Null));
}

#[test]
fn any_value_ungrouped() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE avu (v INT)").unwrap();
    e.execute("INSERT INTO avu VALUES (42), (43)").unwrap();
    let got = rows(&mut e, "SELECT any_value(v) FROM avu");
    assert_eq!(got.len(), 1);
    match got[0][0] {
        spg_storage::Value::Int(n) => assert!(n == 42 || n == 43),
        ref other => panic!("got {other:?}"),
    }
}
