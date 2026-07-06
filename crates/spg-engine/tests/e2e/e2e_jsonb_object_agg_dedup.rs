//! jsonb_object_agg dedups duplicate keys keeping the last value;
//! json_object_agg preserves every pair.

use spg_engine::{Engine, QueryResult};

fn json_text(e: &mut Engine, sql: &str) -> String {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::Json(s) => s.to_string(),
        other => panic!("{sql}: expected Json, got {other:?}"),
    }
}

fn setup() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE oa (g TEXT, v INT)").unwrap();
    e.execute("INSERT INTO oa VALUES ('a',10),('a',20),('b',30)")
        .unwrap();
    e
}

#[test]
fn jsonb_dedups_last_wins() {
    let mut e = setup();
    // jsonb is a map: the duplicate "a" keeps the last value (20).
    let got = json_text(&mut e, "SELECT jsonb_object_agg(g, v) FROM oa ORDER BY 1");
    assert_eq!(got, "{\"a\": 20, \"b\": 30}");
}

#[test]
fn json_preserves_duplicates() {
    let mut e = setup();
    // json keeps every pair, duplicates and all.
    let got = json_text(&mut e, "SELECT json_object_agg(g, v) FROM oa");
    assert_eq!(got, "{\"a\": 10, \"a\": 20, \"b\": 30}");
}

#[test]
fn no_duplicates_unchanged() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE oa2 (g TEXT, v INT)").unwrap();
    e.execute("INSERT INTO oa2 VALUES ('x',1),('y',2)").unwrap();
    assert_eq!(
        json_text(&mut e, "SELECT jsonb_object_agg(g, v) FROM oa2"),
        "{\"x\": 1, \"y\": 2}"
    );
}

#[test]
fn jsonb_object_agg_canonicalises_key_order() {
    // jsonb_object_agg emits canonical jsonb — keys sorted regardless of
    // insertion order (live PG18.4: {"a": 1, "b": 2}). json_object_agg
    // keeps first-seen order.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ord (k TEXT, v INT)").unwrap();
    e.execute("INSERT INTO ord VALUES ('b',2),('a',1),('c',3)").unwrap();
    assert_eq!(
        json_text(&mut e, "SELECT jsonb_object_agg(k, v) FROM ord"),
        "{\"a\": 1, \"b\": 2, \"c\": 3}"
    );
    // json_object_agg keeps insertion order.
    assert_eq!(
        json_text(&mut e, "SELECT json_object_agg(k, v) FROM ord"),
        "{\"b\": 2, \"a\": 1, \"c\": 3}"
    );
    // jsonb_agg canonicalises nested object keys.
    assert_eq!(
        json_text(
            &mut e,
            "SELECT jsonb_agg(x) FROM (VALUES ('{\"b\":2,\"a\":1}'::jsonb)) t(x)"
        ),
        "[{\"a\": 1, \"b\": 2}]"
    );
}
