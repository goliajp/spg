//! read01 round 412 (MySQL differential) — value-picking comparators fold
//! text by the session collation.
//!
//! MariaDB's default collation `utf8mb4_uca1400_ai_ci` is case- and
//! accent-insensitive, PAD SPACE. Three "value-picking" comparators
//! deduplicated / picked using byte-exact ordering under MySQL and returned
//! a silent-wrong value:
//!   MIN / MAX             — over {Zebra, apple, Mango} SPG=Mango,apple
//!                            (MariaDB: apple, Zebra)
//!   GREATEST / LEAST      — LEAST('a','B','c') SPG=B (MariaDB: a)
//!   CASE op WHEN v ...    — CASE 'A' WHEN 'a' SPG='no' (MariaDB: 'match')
//!
//! Each path now folds text via `mysql_compare_fold` under the MySQL dialect,
//! matching ORDER BY (round 411). PostgreSQL sessions keep byte-exact
//! semantics.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn scalar(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Null => "NULL".to_string(),
            v => spg_engine::eval::value_to_text(v),
        },
        other => panic!("{other:?}"),
    }
}

fn pair(e: &mut Engine, sql: &str) -> (String, String) {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => {
            let r = &rows[0].values;
            (spg_engine::eval::value_to_text(&r[0]), spg_engine::eval::value_to_text(&r[1]))
        }
        other => panic!("{other:?}"),
    }
}

/// MIN / MAX fold text (apple < Émile < Mango < Zebra).
#[test]
fn min_max_folds() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(v VARCHAR(10))").unwrap();
    e.execute("INSERT INTO t VALUES('Zebra'),('apple'),('Mango'),('Émile')").unwrap();
    assert_eq!(pair(&mut e, "SELECT MIN(v), MAX(v) FROM t"),
               ("apple".to_string(), "Zebra".to_string()));
}

/// GREATEST / LEAST fold text (a < B < c under fold).
#[test]
fn greatest_least_folds() {
    let mut e = mysql();
    assert_eq!(pair(&mut e, "SELECT LEAST('a','B','c'), GREATEST('a','B','c')"),
               ("a".to_string(), "c".to_string()));
    assert_eq!(
        pair(&mut e, "SELECT LEAST('Zebra','apple','Mango'), GREATEST('Zebra','apple','Mango')"),
        ("apple".to_string(), "Zebra".to_string())
    );
    assert_eq!(
        pair(&mut e, "SELECT LEAST('Émile','apple','Mango'), GREATEST('Émile','apple','Mango')"),
        ("apple".to_string(), "Mango".to_string())
    );
}

/// CASE op WHEN v folds text (case, accent, trailing space).
#[test]
fn case_when_folds() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT CASE 'A' WHEN 'a' THEN 'match' ELSE 'no' END"), "match");
    assert_eq!(scalar(&mut e, "SELECT CASE 'é' WHEN 'e' THEN 'match' ELSE 'no' END"), "match");
    assert_eq!(scalar(&mut e, "SELECT CASE 'A ' WHEN 'a' THEN 'match' ELSE 'no' END"), "match");
    // A downstream WHEN branch also folds.
    assert_eq!(
        scalar(&mut e, "SELECT CASE 'foo' WHEN 'bar' THEN 1 WHEN 'FOO' THEN 2 ELSE 0 END"),
        "2"
    );
}

/// A PostgreSQL session keeps byte-exact ordering (uppercase < lowercase).
#[test]
fn postgres_byte_exact() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(v VARCHAR(10))").unwrap();
    e.execute("INSERT INTO t VALUES('Zebra'),('apple'),('Mango')").unwrap();
    assert_eq!(pair(&mut e, "SELECT MIN(v), MAX(v) FROM t"),
               ("Mango".to_string(), "apple".to_string()));
    assert_eq!(pair(&mut e, "SELECT least('a','B','c'), greatest('a','B','c')"),
               ("B".to_string(), "c".to_string()));
    assert_eq!(scalar(&mut e, "SELECT CASE 'A' WHEN 'a' THEN 'match' ELSE 'no' END"), "no");
}
