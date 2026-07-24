//! read01 round 403 (MySQL differential) — NULL sorts FIRST for ASC /
//! LAST for DESC under the MySQL dialect.
//!
//! MySQL treats NULL as the smallest value, so `ORDER BY v` (ascending)
//! puts the NULLs first and `ORDER BY v DESC` puts them last. PostgreSQL's
//! default is the reverse (NULLS LAST for ASC), which is what SPG did, so a
//! MySQL query on a nullable column got the rows in the wrong order — a
//! silent-wrong. An explicit `NULLS FIRST` / `NULLS LAST` still wins, and a
//! PostgreSQL session keeps the PG default.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

/// The column values with NULL rendered as the string "NULL".
fn seq(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                Value::Null => "NULL".to_string(),
                Value::Int(n) => n.to_string(),
                Value::Text(s) => s.to_string(),
                o => panic!("{o:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn setup() -> Engine {
    let mut e = mysql();
    e.execute("CREATE TABLE n(v INT, s VARCHAR(5))").unwrap();
    e.execute("INSERT INTO n VALUES (3,NULL),(NULL,'b'),(1,'a'),(NULL,NULL),(2,'c')")
        .unwrap();
    e
}

/// ASC puts NULLs first, DESC puts them last.
#[test]
fn null_first_asc_last_desc() {
    let mut e = setup();
    assert_eq!(
        seq(&mut e, "SELECT v FROM n ORDER BY v ASC"),
        vec!["NULL", "NULL", "1", "2", "3"]
    );
    assert_eq!(
        seq(&mut e, "SELECT v FROM n ORDER BY v DESC"),
        vec!["3", "2", "1", "NULL", "NULL"]
    );
}

/// Text columns place NULL first too.
#[test]
fn text_null_first() {
    let mut e = setup();
    assert_eq!(
        seq(&mut e, "SELECT s FROM n ORDER BY s ASC"),
        vec!["NULL", "NULL", "a", "b", "c"]
    );
}

/// An explicit NULLS FIRST / LAST still wins.
#[test]
fn explicit_nulls_clause_wins() {
    let mut e = setup();
    assert_eq!(
        seq(&mut e, "SELECT v FROM n ORDER BY v ASC NULLS LAST"),
        vec!["1", "2", "3", "NULL", "NULL"]
    );
    assert_eq!(
        seq(&mut e, "SELECT v FROM n ORDER BY v DESC NULLS FIRST"),
        vec!["NULL", "NULL", "3", "2", "1"]
    );
}

/// A PostgreSQL session keeps the PG default (NULLS LAST for ASC).
#[test]
fn postgres_default_nulls_last_asc() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE n(v INT)").unwrap();
    e.execute("INSERT INTO n VALUES (3),(NULL),(1),(NULL),(2)")
        .unwrap();
    assert_eq!(
        seq(&mut e, "SELECT v FROM n ORDER BY v ASC"),
        vec!["1", "2", "3", "NULL", "NULL"]
    );
}
