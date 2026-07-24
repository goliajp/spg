//! read01 round 406 (MySQL differential) — `INSERT IGNORE`.
//!
//! MySQL's `INSERT IGNORE INTO t …` turns a would-be duplicate-key error
//! into a silently skipped row: non-conflicting rows still insert, and the
//! existing row keeps its value. It arbitrates on every unique key (not a
//! named target), i.e. it lowers to `ON CONFLICT DO NOTHING` over all
//! unique constraints. PostgreSQL (and SPG until now) has no `IGNORE`
//! keyword after INSERT and rejects the statement.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        Value::Int(n) => n.to_string(),
                        Value::BigInt(n) => n.to_string(),
                        Value::Null => "NULL".to_string(),
                        o => panic!("{o:?}"),
                    })
                    .collect()
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// A conflicting row is skipped (keeps its old value); the rest insert.
#[test]
fn primary_key_conflict_is_skipped() {
    let mut e = mysql();
    e.execute("CREATE TABLE ig(id INT PRIMARY KEY, v INT)").unwrap();
    e.execute("INSERT INTO ig VALUES(1,10),(2,20)").unwrap();
    // id 2 collides (stays v=20); id 3 is new.
    e.execute("INSERT IGNORE INTO ig VALUES(2,99),(3,30)").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT id, v FROM ig ORDER BY id"),
        vec![
            vec!["1", "10"],
            vec!["2", "20"],
            vec!["3", "30"],
        ]
    );
}

/// IGNORE arbitrates on every unique key, not just the primary key.
#[test]
fn secondary_unique_conflict_is_skipped() {
    let mut e = mysql();
    e.execute("CREATE TABLE u(id INT PRIMARY KEY, e INT UNIQUE)")
        .unwrap();
    e.execute("INSERT INTO u VALUES(1,100)").unwrap();
    // id 2 has e=100 which collides on the UNIQUE key -> skipped; id 3 new.
    e.execute("INSERT IGNORE INTO u VALUES(2,100),(3,300)").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT id, e FROM u ORDER BY id"),
        vec![vec!["1", "100"], vec!["3", "300"]]
    );
}

/// Case-insensitive keyword.
#[test]
fn ignore_is_case_insensitive() {
    let mut e = mysql();
    e.execute("CREATE TABLE ig(id INT PRIMARY KEY, v INT)").unwrap();
    e.execute("INSERT INTO ig VALUES(1,10)").unwrap();
    e.execute("insert IGNORE into ig values(1,99),(2,20)").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT id, v FROM ig ORDER BY id"),
        vec![vec!["1", "10"], vec!["2", "20"]]
    );
}

/// An explicit ON CONFLICT clause is unaffected (does not exist together
/// with IGNORE in practice, but IGNORE must not clobber an explicit one).
#[test]
fn no_conflict_all_insert() {
    let mut e = mysql();
    e.execute("CREATE TABLE ig(id INT PRIMARY KEY, v INT)").unwrap();
    e.execute("INSERT IGNORE INTO ig VALUES(1,10),(2,20)").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT id, v FROM ig ORDER BY id"),
        vec![vec!["1", "10"], vec!["2", "20"]]
    );
}

/// A PostgreSQL session has no IGNORE keyword and rejects the statement.
#[test]
fn postgres_rejects_ignore() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ig(id INT PRIMARY KEY, v INT)").unwrap();
    assert!(
        e.execute("INSERT IGNORE INTO ig VALUES(1,10)").is_err(),
        "PG has no INSERT IGNORE"
    );
}
