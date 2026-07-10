//! v7.37.17 (17.6 siblings) — MySQL group_concat + SQL/XML xmlagg.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter()
        .map(|row| row.values.into_iter().collect())
        .collect()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn group_concat_comma_default() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE gc (g INT, v TEXT)").unwrap();
    e.execute("INSERT INTO gc VALUES (1, 'a'), (1, 'b'), (2, 'c')")
        .unwrap();
    let got = rows(
        &mut e,
        "SELECT g, group_concat(v) FROM gc GROUP BY g ORDER BY g",
    );
    assert_eq!(text(&got[0][1]), "a,b");
    assert_eq!(text(&got[1][1]), "c");
}

#[test]
fn group_concat_coerces_numbers() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE gcn (v INT)").unwrap();
    e.execute("INSERT INTO gcn VALUES (1), (2), (3)").unwrap();
    let got = rows(&mut e, "SELECT group_concat(v) FROM gcn");
    assert_eq!(text(&got[0][0]), "1,2,3");
}

#[test]
fn xmlagg_bare_concat() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE xa (v TEXT)").unwrap();
    e.execute("INSERT INTO xa VALUES ('<a/>'), ('<b/>')")
        .unwrap();
    let got = rows(&mut e, "SELECT xmlagg(v) FROM xa");
    assert_eq!(text(&got[0][0]), "<a/><b/>");
}

#[test]
fn group_concat_empty_group_is_null() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE gce (v TEXT)").unwrap();
    let got = rows(&mut e, "SELECT group_concat(v) FROM gce");
    assert!(matches!(got[0][0], spg_storage::Value::Null));
}
