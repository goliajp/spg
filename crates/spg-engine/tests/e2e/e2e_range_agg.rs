//! v7.37.17 (17.6 siblings) — PG 14+ range_agg: collect ranges
//! into a multirange.

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
fn range_agg_collects_ranges() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE spans (lo INT, hi INT)").unwrap();
    e.execute("INSERT INTO spans VALUES (1, 3), (5, 8)")
        .unwrap();
    let got = text(&first(
        &mut e,
        "SELECT range_agg(int4range(lo, hi))::text FROM spans",
    ));
    assert_eq!(got, "{[1,3),[5,8)}");
}

#[test]
fn range_agg_group_by() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE g (k TEXT, lo INT, hi INT)")
        .unwrap();
    e.execute("INSERT INTO g VALUES ('a', 1, 2), ('a', 4, 6), ('b', 10, 20)")
        .unwrap();
    let r = e
        .execute(
            "SELECT k, range_agg(int4range(lo, hi))::text FROM g \
             GROUP BY k ORDER BY k",
        )
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    assert_eq!(rows.len(), 2);
    let a = match &rows[0].values[1] {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    };
    assert_eq!(a, "{[1,2),[4,6)}");
    let b = match &rows[1].values[1] {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    };
    assert_eq!(b, "{[10,20)}");
}

#[test]
fn range_agg_all_empty_and_empty_group() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE em (lo INT, hi INT)").unwrap();
    e.execute("INSERT INTO em VALUES (5, 5)").unwrap();
    // All-empty ranges finalize to the empty multirange {}.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT range_agg(int4range(lo, hi))::text FROM em"
        )),
        "{}"
    );
    // Empty group → NULL.
    assert!(matches!(
        first(
            &mut e,
            "SELECT range_agg(int4range(lo, hi)) FROM em WHERE lo > 100"
        ),
        spg_storage::Value::Null
    ));
}
