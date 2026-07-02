//! v7.37.17 (17.6 siblings) — FROM-position jsonb_object_keys /
//! json_object_keys (the scalar TextArray surface shipped as task
//! #169; this adds the SRF form via the unnest rewrite).

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

fn texts(got: &[Vec<spg_storage::Value<'static>>]) -> Vec<String> {
    got.iter()
        .map(|r| match &r[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => panic!("expected Text, got {other:?}"),
        })
        .collect()
}

#[test]
fn from_jsonb_object_keys_rows() {
    let mut e = Engine::new();
    // PG doc vector: jsonb_object_keys('{"f1":"abc","f2":{"f3":"a"}}')
    // → f1 / f2. The natural column name is the function name.
    let got = rows(
        &mut e,
        "SELECT jsonb_object_keys \
         FROM jsonb_object_keys('{\"f1\": \"abc\", \"f2\": {\"f3\": \"a\"}}') \
         ORDER BY 1",
    );
    assert_eq!(texts(&got), ["f1", "f2"]);
}

#[test]
fn column_alias_and_count() {
    let mut e = Engine::new();
    let got = rows(
        &mut e,
        "SELECT k FROM json_object_keys('{\"a\": 1, \"b\": 2}') AS t(k) ORDER BY k",
    );
    assert_eq!(texts(&got), ["a", "b"]);
    let got = rows(
        &mut e,
        "SELECT COUNT(*) FROM jsonb_object_keys('{\"a\": 1, \"b\": 2, \"c\": 3}')",
    );
    assert!(matches!(
        got[0][0],
        spg_storage::Value::Int(3) | spg_storage::Value::BigInt(3)
    ));
}
