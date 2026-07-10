//! v7.38 (read01, T9) — composite field access `(expr).field`: pull a member
//! out of a `ROW(...)` constructor or a whole-row reference. Only the
//! parenthesised form is field access; a bare `a.b` stays a qualified column.
//! Every expected value is from live PG18.4.

use spg_engine::{Engine, QueryResult};

fn row1(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(|v| match v {
                spg_storage::Value::Text(s) => s.to_string(),
                spg_storage::Value::Int(n) => n.to_string(),
                spg_storage::Value::BigInt(n) => n.to_string(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

#[test]
fn field_access_on_row_constructor() {
    let mut e = Engine::new();
    assert_eq!(row1(&mut e, "SELECT (ROW(1,2)).f1, (ROW(1,2)).f2"), vec!["1", "2"]);
}

#[test]
fn field_access_on_whole_row_alias() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE emp(id int, name text)").unwrap();
    e.execute("INSERT INTO emp VALUES (1,'a')").unwrap();
    assert_eq!(row1(&mut e, "SELECT (e).id, (e).name FROM emp e"), vec!["1", "a"]);
}

#[test]
fn field_access_composes_in_expressions() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE emp(id int, name text)").unwrap();
    e.execute("INSERT INTO emp VALUES (5,'a')").unwrap();
    // Field access nests inside arithmetic and a cast.
    assert_eq!(
        row1(&mut e, "SELECT (e).id + 10, ((e).id)::text FROM emp e"),
        vec!["15", "5"]
    );
}

#[test]
fn field_access_on_subquery_alias() {
    let mut e = Engine::new();
    assert_eq!(row1(&mut e, "SELECT (r).a, (r).b FROM (SELECT 1 a, 2 b) r"), vec!["1", "2"]);
}
