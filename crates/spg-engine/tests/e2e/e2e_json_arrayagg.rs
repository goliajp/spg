//! v7.37.17 (17.6 siblings) — SQL:2016 json_arrayagg /
//! json_objectagg spellings (PG 16+ aliases).

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
    assert_eq!(text_or_json(&std_form[0][0]), text_or_json(&pg_form[0][0]),);
}

#[test]
fn json_objectagg_matches_json_object_agg() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE jo (k TEXT, v INT)").unwrap();
    e.execute("INSERT INTO jo VALUES ('a', 1), ('b', 2)")
        .unwrap();
    let std_form = rows(&mut e, "SELECT json_objectagg(k, v) FROM jo");
    let pg_form = rows(&mut e, "SELECT json_object_agg(k, v) FROM jo");
    assert_eq!(text_or_json(&std_form[0][0]), text_or_json(&pg_form[0][0]),);
}

#[test]
fn json_agg_honours_order_by() {
    // The aggregate's own ORDER BY reorders the elements (live PG18.4:
    // json_agg(x ORDER BY x DESC) → [3, 2, 1]). Previously json_agg
    // ignored the ORDER BY (unlike string_agg / array_agg) because it
    // never recorded the sort keys.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ja (v INT, k TEXT)").unwrap();
    e.execute("INSERT INTO ja VALUES (2,'b'),(1,'a'),(3,'c')")
        .unwrap();
    assert_eq!(
        text_or_json(&rows(&mut e, "SELECT json_agg(v ORDER BY v DESC) FROM ja")[0][0]),
        "[3, 2, 1]"
    );
    // Order by a different column (k: a→1, b→2, c→3).
    assert_eq!(
        text_or_json(&rows(&mut e, "SELECT json_agg(v ORDER BY k) FROM ja")[0][0]),
        "[1, 2, 3]"
    );
    // jsonb variant + no-ORDER-BY control.
    assert_eq!(
        text_or_json(&rows(&mut e, "SELECT jsonb_agg(v ORDER BY v) FROM ja")[0][0]),
        "[1, 2, 3]"
    );
    assert_eq!(
        text_or_json(&rows(&mut e, "SELECT json_agg(v ORDER BY v) FROM ja")[0][0]),
        "[1, 2, 3]"
    );
}
