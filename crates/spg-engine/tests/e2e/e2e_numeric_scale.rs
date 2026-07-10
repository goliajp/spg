//! v7.37.17 (17.6 siblings) — PG 13+ scale / min_scale / trim_scale.

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

fn as_int(v: &spg_storage::Value<'_>) -> i32 {
    match v {
        spg_storage::Value::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn scale_reports_declared_scale() {
    let mut e = Engine::new();
    // Integer input → scale 0.
    assert_eq!(as_int(&first(&mut e, "SELECT scale(42)")), 0);
}

#[test]
fn min_scale_strips_trailing_zeroes() {
    let mut e = Engine::new();
    // Integers have min_scale 0.
    assert_eq!(as_int(&first(&mut e, "SELECT min_scale(42)")), 0);
    // Numeric column path: create a table with NUMERIC to exercise
    // the Numeric variant.
    e.execute("CREATE TABLE ns (v NUMERIC(10, 4))").unwrap();
    e.execute("INSERT INTO ns VALUES (3.1000), (2.5000), (7.0000)")
        .unwrap();
    let r = e.execute("SELECT min_scale(v) FROM ns ORDER BY v").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    // 2.5000 → min_scale 1; 3.1000 → 1; 7.0000 → 0.
    let scales: Vec<i32> = rows
        .iter()
        .map(|row| match &row.values[0] {
            spg_storage::Value::Int(n) => *n,
            other => panic!("got {other:?}"),
        })
        .collect();
    assert_eq!(scales, [1, 1, 0]);
}

#[test]
fn trim_scale_removes_trailing_zeroes() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ts (v NUMERIC(10, 4))").unwrap();
    e.execute("INSERT INTO ts VALUES (3.1000)").unwrap();
    let r = e.execute("SELECT trim_scale(v) FROM ts").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::Numeric { scaled, scale, .. } => {
            assert_eq!(*scaled, 31);
            assert_eq!(*scale, 1);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn scale_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "scale(NULL::numeric)",
        "min_scale(NULL::numeric)",
        "trim_scale(NULL::numeric)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
