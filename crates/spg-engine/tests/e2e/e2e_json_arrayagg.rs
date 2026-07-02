//! v7.37.17 (17.6 siblings) — SQL:2016 json_arrayagg /
//! json_objectagg spellings (PG 16+ aliases).

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

fn text_or_json(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        spg_storage::Value::Json(s) => s.to_string(),
        other => panic!("expected Text/Json, got {other:?}"),
    }
}

#[test]
fn json_arrayagg_matches_json_agg() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ja (v INT)").unwrap();
    e.execute("INSERT INTO ja VALUES (1), (2), (3)").unwrap();
    let std_form = rows(&mut e, "SELECT json_arrayagg(v) FROM ja");
    let pg_form = rows(&mut e, "SELECT json_agg(v) FROM ja");
    assert_eq!(
        text_or_json(&std_form[0][0]),
        text_or_json(&pg_form[0][0]),
    );
}

#[test]
fn json_objectagg_matches_json_object_agg() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE jo (k TEXT, v INT)").unwrap();
    e.execute("INSERT INTO jo VALUES ('a', 1), ('b', 2)")
        .unwrap();
    let std_form = rows(&mut e, "SELECT json_objectagg(k, v) FROM jo");
    let pg_form = rows(&mut e, "SELECT json_object_agg(k, v) FROM jo");
    assert_eq!(
        text_or_json(&std_form[0][0]),
        text_or_json(&pg_form[0][0]),
    );
}
