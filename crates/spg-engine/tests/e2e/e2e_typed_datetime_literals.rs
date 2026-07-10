//! v7.37.17 (17.6 siblings) — PG typed datetime literals:
//! DATE '...' / TIMESTAMP '...' / TIMESTAMPTZ '...' lower onto the
//! ::cast runtime paths.

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

#[test]
fn date_literal() {
    let mut e = Engine::new();
    let direct = first(&mut e, "SELECT DATE '2003-01-02'");
    let cast = first(&mut e, "SELECT '2003-01-02'::date");
    assert!(matches!(direct, spg_storage::Value::Date(_)));
    assert_eq!(direct, cast);
}

#[test]
fn timestamp_literal() {
    let mut e = Engine::new();
    let direct = first(&mut e, "SELECT TIMESTAMP '2003-01-02 10:30:00'");
    let cast = first(&mut e, "SELECT '2003-01-02 10:30:00'::timestamp");
    assert!(matches!(direct, spg_storage::Value::Timestamp(_)));
    assert_eq!(direct, cast);
}

#[test]
fn composes_in_expressions() {
    let mut e = Engine::new();
    // The #318 shape that exposed the gap.
    assert!(matches!(
        first(
            &mut e,
            "SELECT timestampdiff(MONTH, DATE '2003-02-01', DATE '2003-05-01')"
        ),
        spg_storage::Value::BigInt(3)
    ));
    // Comparison context.
    assert!(matches!(
        first(&mut e, "SELECT DATE '2003-01-02' < DATE '2003-01-03'"),
        spg_storage::Value::Bool(true)
    ));
}

#[test]
fn bare_date_ident_stays_a_column() {
    let mut e = Engine::new();
    // A column named `date` must keep working when no string
    // literal follows.
    e.execute("CREATE TABLE t (date TEXT)").unwrap();
    e.execute("INSERT INTO t VALUES ('x')").unwrap();
    let got = first(&mut e, "SELECT date FROM t");
    assert!(matches!(got, spg_storage::Value::Text(ref s) if s == "x"));
}
