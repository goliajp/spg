//! read01 round 391 (MySQL differential) — `JSON_OBJECT(k1, v1, k2, v2, …)`
//! is the variadic key/value constructor under the MySQL dialect.
//!
//! MySQL's `JSON_OBJECT` takes alternating key/value arguments
//! (`JSON_OBJECT('k', 1, 'v', 2)` is `{"k": 1, "v": 2}`). SPG mapped the
//! name to PostgreSQL's array-based `json_object(text[] [, text[]])`, so a
//! MySQL call errored with "json_object() takes 1 or 2 args". Under the
//! MySQL dialect it now routes to the variadic constructor (PG's
//! `json_build_object`), canonicalised to MariaDB's render. A NULL value is
//! kept (`{"a": null}`), and a PostgreSQL session keeps the array form.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn json(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Json(j) => j.to_string(),
            other => panic!("`{sql}` not json: {other:?}"),
        },
        other => panic!("`{sql}`: {other:?}"),
    }
}

/// Alternating key/value args build the object, byte-identical to MariaDB.
#[test]
fn json_object_variadic() {
    let mut e = mysql();
    assert_eq!(json(&mut e, "SELECT JSON_OBJECT('k', 1, 'v', 2)"), r#"{"k": 1, "v": 2}"#);
    assert_eq!(json(&mut e, "SELECT JSON_OBJECT()"), "{}");
}

/// A NULL value is kept as JSON null (not the whole object → NULL).
#[test]
fn null_value_kept() {
    let mut e = mysql();
    assert_eq!(json(&mut e, "SELECT JSON_OBJECT('a', NULL)"), r#"{"a": null}"#);
}

/// A nested JSON value nests.
#[test]
fn nested_value() {
    let mut e = mysql();
    assert_eq!(
        json(&mut e, "SELECT JSON_OBJECT('a', JSON_ARRAY(1, 2))"),
        r#"{"a": [1, 2]}"#
    );
}

/// JSON_TYPE of the result is OBJECT.
#[test]
fn result_is_an_object() {
    let mut e = mysql();
    match e.execute("SELECT JSON_TYPE(JSON_OBJECT('a', 1))").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], Value::text("OBJECT"));
        }
        other => panic!("{other:?}"),
    }
}

/// A PostgreSQL session keeps the array-based `json_object`.
#[test]
fn postgres_array_form_unchanged() {
    let mut e = Engine::new();
    assert_eq!(
        json(&mut e, "SELECT json_object(ARRAY['a', '1', 'b', '2'])"),
        r#"{"a" : "1", "b" : "2"}"#
    );
}
