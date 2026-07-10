//! v7.37.17 (17.6 siblings) — PG 14+ range_intersect_agg:
//! intersection fold over ranges.

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
fn overlapping_ranges_intersect() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE iv (lo INT, hi INT)").unwrap();
    // [1,10) ∩ [5,20) ∩ [3,8) = [5,8).
    e.execute("INSERT INTO iv VALUES (1, 10), (5, 20), (3, 8)")
        .unwrap();
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT range_intersect_agg(int4range(lo, hi))::text FROM iv"
        )),
        "[5,8)"
    );
}

#[test]
fn disjoint_ranges_collapse_to_empty() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE dj (lo INT, hi INT)").unwrap();
    e.execute("INSERT INTO dj VALUES (1, 3), (5, 8)").unwrap();
    assert!(matches!(
        first(
            &mut e,
            "SELECT isempty(range_intersect_agg(int4range(lo, hi))) FROM dj"
        ),
        spg_storage::Value::Bool(true)
    ));
}

#[test]
fn unbounded_side_loses_to_bounded() {
    let mut e = Engine::new();
    // (-inf, 10) ∩ [5, +inf) = [5, 10) — NULL columns make the
    // unbounded sides per-row.
    e.execute("CREATE TABLE ub (lo INT, hi INT)").unwrap();
    e.execute("INSERT INTO ub VALUES (NULL, 10), (5, NULL)")
        .unwrap();
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT range_intersect_agg(int4range(lo, hi))::text FROM ub"
        )),
        "[5,10)"
    );
}

#[test]
fn empty_group_is_null() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE eg (lo INT, hi INT)").unwrap();
    assert!(matches!(
        first(
            &mut e,
            "SELECT range_intersect_agg(int4range(lo, hi)) FROM eg"
        ),
        spg_storage::Value::Null
    ));
}
