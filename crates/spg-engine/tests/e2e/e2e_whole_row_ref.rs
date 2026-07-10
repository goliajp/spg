//! v7.38 (read01, T9) — whole-row reference: a bare name equal to a FROM
//! alias (real table or subquery) resolves to the composite record of every
//! column. Powers `row_to_json(e)` / `to_jsonb(e)` and a bare `SELECT e`.
//! Every expected value is from live PG18.4.

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Text(s) => s.to_string(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

fn setup() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE emp(id int, name text)").unwrap();
    e.execute("INSERT INTO emp VALUES (1,'a'),(2,'b')").unwrap();
    e
}

#[test]
fn row_to_json_of_table_alias() {
    let mut e = setup();
    assert_eq!(
        col(&mut e, "SELECT row_to_json(x)::text FROM emp x ORDER BY x.id"),
        vec!["{\"id\":1,\"name\":\"a\"}", "{\"id\":2,\"name\":\"b\"}"]
    );
}

#[test]
fn row_to_json_of_subquery_alias() {
    let mut e = setup();
    assert_eq!(
        col(&mut e, "SELECT row_to_json(r)::text FROM (SELECT 1 a, 2 b) r"),
        vec!["{\"a\":1,\"b\":2}"]
    );
}

#[test]
fn to_jsonb_of_table_alias() {
    let mut e = setup();
    assert_eq!(
        col(&mut e, "SELECT to_jsonb(x)::text FROM emp x ORDER BY x.id"),
        vec!["{\"id\": 1, \"name\": \"a\"}", "{\"id\": 2, \"name\": \"b\"}"]
    );
}

#[test]
fn bare_whole_row_renders_as_record_text() {
    let mut e = setup();
    assert_eq!(
        col(&mut e, "SELECT x::text FROM emp x ORDER BY x.id"),
        vec!["(1,a)", "(2,b)"]
    );
}
