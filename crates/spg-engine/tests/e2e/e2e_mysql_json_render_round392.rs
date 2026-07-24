//! read01 round 392 (MySQL differential) — the JSON mutation / merge family
//! renders in MariaDB's canonical spacing (`{"a": 1, "b": 2}`).
//!
//! MariaDB serialises JSON with `": "` after each key and `", "` between
//! members (`{"a": 1, "b": 2}`, `[1, 2, 3]`). SPG's JSON_SET / INSERT /
//! REPLACE / REMOVE / ARRAY_APPEND / ARRAY_INSERT / MERGE emitted the
//! compact form (`{"a":1,"b":2}`), so their output diverged from MariaDB
//! byte-for-byte. They now canonicalise, like JSON_OBJECT (r391).
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

#[test]
fn set_insert_replace() {
    let mut e = mysql();
    assert_eq!(json(&mut e, r#"SELECT JSON_SET('{"a":1}','$.b',2)"#), r#"{"a": 1, "b": 2}"#);
    assert_eq!(json(&mut e, r#"SELECT JSON_INSERT('{"a":1}','$.b',2)"#), r#"{"a": 1, "b": 2}"#);
    assert_eq!(json(&mut e, r#"SELECT JSON_REPLACE('{"a":1}','$.a',9)"#), r#"{"a": 9}"#);
}

#[test]
fn remove() {
    let mut e = mysql();
    assert_eq!(json(&mut e, r#"SELECT JSON_REMOVE('{"a":1,"b":2}','$.a')"#), r#"{"b": 2}"#);
}

#[test]
fn merge() {
    let mut e = mysql();
    assert_eq!(
        json(&mut e, r#"SELECT JSON_MERGE_PRESERVE('[1,2]','[3,4]')"#),
        "[1, 2, 3, 4]"
    );
    assert_eq!(
        json(&mut e, r#"SELECT JSON_MERGE_PATCH('{"a":1}','{"b":2}')"#),
        r#"{"a": 1, "b": 2}"#
    );
}

#[test]
fn array_append_insert() {
    let mut e = mysql();
    assert_eq!(json(&mut e, r#"SELECT JSON_ARRAY_APPEND('[1,2]','$',3)"#), "[1, 2, 3]");
    assert_eq!(json(&mut e, r#"SELECT JSON_ARRAY_INSERT('[1,2]','$[0]',9)"#), "[9, 1, 2]");
}

/// A nested document keeps the spacing at every level.
#[test]
fn nested() {
    let mut e = mysql();
    assert_eq!(
        json(&mut e, r#"SELECT JSON_REPLACE('{"a":1,"c":{"d":3}}','$.a',9)"#),
        r#"{"a": 9, "c": {"d": 3}}"#
    );
}
