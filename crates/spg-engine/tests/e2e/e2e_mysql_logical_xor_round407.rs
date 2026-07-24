//! read01 round 407 (MySQL differential) — logical `XOR` operator.
//!
//! MySQL has a logical exclusive-or operator `XOR`: it reads both sides as
//! truth values and returns 1 when exactly one side is true, 0 otherwise,
//! and NULL when either side is NULL. Its precedence sits between OR (the
//! loosest) and AND, so `1 OR 0 XOR 1` is `1 OR (0 XOR 1)` and
//! `1 XOR 1 AND 0` is `1 XOR (1 AND 0)`. NOT binds tighter than XOR, so
//! `NOT 0 XOR 1` is `(NOT 0) XOR 1`. PostgreSQL has no logical XOR and
//! rejects the keyword.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

/// Render like MariaDB's `mysql -N`: booleans as 1 / 0, NULL as "NULL".
fn scalar(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => {
            let v = &rows[0].values[0];
            match v {
                Value::Bool(b) => (if *b { "1" } else { "0" }).to_string(),
                Value::Int(n) => n.to_string(),
                Value::BigInt(n) => n.to_string(),
                Value::Null => "NULL".to_string(),
                o => panic!("{o:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
}

/// Basic truth table, matching MariaDB (1 XOR 1 = 0, 1 XOR 0 = 1).
#[test]
fn truth_table() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT 1 XOR 1"), "0");
    assert_eq!(scalar(&mut e, "SELECT 1 XOR 0"), "1");
    assert_eq!(scalar(&mut e, "SELECT 0 XOR 1"), "1");
    assert_eq!(scalar(&mut e, "SELECT 0 XOR 0"), "0");
    // A non-zero number is true.
    assert_eq!(scalar(&mut e, "SELECT 5 XOR 0"), "1");
}

/// NULL on either side yields NULL.
#[test]
fn null_propagates() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT NULL XOR 1"), "NULL");
    assert_eq!(scalar(&mut e, "SELECT 1 XOR NULL"), "NULL");
    assert_eq!(scalar(&mut e, "SELECT NULL XOR NULL"), "NULL");
}

/// A non-numeric string reads as false; a leading-numeric string reads as
/// its number's truthiness.
#[test]
fn string_truthiness() {
    let mut e = mysql();
    // 'a' -> 0, '' -> 0  => 0 XOR 0 = 0
    assert_eq!(scalar(&mut e, "SELECT 'a' XOR ''"), "0");
    // '5abc' -> 5 (true), '0' -> 0 (false) => 1
    assert_eq!(scalar(&mut e, "SELECT '5abc' XOR '0'"), "1");
}

/// XOR is left-associative: `1 XOR 1 XOR 1` = `(1 XOR 1) XOR 1` = 1.
#[test]
fn left_associative() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT 1 XOR 1 XOR 1"), "1");
}

/// Precedence: OR is looser than XOR; AND is tighter; NOT is tighter still.
#[test]
fn precedence() {
    let mut e = mysql();
    // OR < XOR: 1 OR (0 XOR 1) = 1 OR 1 = 1.
    assert_eq!(scalar(&mut e, "SELECT 1 OR 0 XOR 1"), "1");
    // AND > XOR: 1 XOR (1 AND 0) = 1 XOR 0 = 1.
    assert_eq!(scalar(&mut e, "SELECT 1 XOR 1 AND 0"), "1");
    // NOT > XOR: (NOT 0) XOR 1 = 1 XOR 1 = 0.
    assert_eq!(scalar(&mut e, "SELECT NOT 0 XOR 1"), "0");
}

/// XOR in a WHERE clause (exercises the compiled predicate path).
#[test]
fn where_filter() {
    let mut e = mysql();
    e.execute("CREATE TABLE xt(a INT, b INT)").unwrap();
    e.execute("INSERT INTO xt VALUES(1,0),(1,1),(0,0),(0,1)")
        .unwrap();
    let out = match e
        .execute("SELECT a, b FROM xt WHERE a XOR b ORDER BY a, b")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        Value::Int(n) => n.to_string(),
                        o => panic!("{o:?}"),
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    // Exactly one of a, b truthy.
    assert_eq!(out, vec![vec!["0", "1"], vec!["1", "0"]]);
}

/// A PostgreSQL session has no logical XOR and rejects the keyword.
#[test]
fn postgres_rejects_xor() {
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT 1 XOR 1").is_err(),
        "PG has no logical XOR"
    );
}
