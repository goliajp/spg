//! read01 round 402 (MySQL differential) — an inline ENUM column reads as
//! its 1-based ordinal in a numeric context.
//!
//! MySQL reads an ENUM value as its position in the declared variant list
//! when it is used numerically: `e + 0` is 1 for the first member, 2 for
//! the second, and so on (`WHERE e + 0 > 1` filters by ordinal). SPG stored
//! the ENUM as text and the arithmetic path coerced it to 0, so `e + 0` was
//! 0 for every row — a silent-wrong. A plain read keeps the text, and a
//! comparison stays a string compare (which is what MariaDB does for
//! `e < 'high'`, matching already).
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn setup() -> Engine {
    let mut e = mysql();
    e.execute("CREATE TABLE en(e ENUM('low','mid','high'))")
        .unwrap();
    e.execute("INSERT INTO en VALUES ('high'),('low'),('mid')")
        .unwrap();
    e
}

fn ints(e: &mut Engine, sql: &str) -> Vec<i64> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                Value::BigInt(n) => *n,
                Value::Int(n) => i64::from(*n),
                o => panic!("{o:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn texts(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                Value::Text(s) => s.to_string(),
                o => panic!("{o:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// `e + 0` / `e * 1` is the 1-based ordinal.
#[test]
fn numeric_context_is_ordinal() {
    let mut e = setup();
    assert_eq!(
        ints(&mut e, "SELECT e + 0 FROM en ORDER BY e"),
        vec![1, 2, 3]
    );
    assert_eq!(
        ints(&mut e, "SELECT e * 1 FROM en ORDER BY e"),
        vec![1, 2, 3]
    );
}

/// `WHERE e + 0 <op> n` filters by ordinal (the compiled filter path).
#[test]
fn where_by_ordinal() {
    let mut e = setup();
    assert_eq!(
        texts(&mut e, "SELECT e FROM en WHERE e + 0 > 1 ORDER BY e"),
        vec!["mid", "high"]
    );
    assert_eq!(
        texts(&mut e, "SELECT e FROM en WHERE e + 0 = 2"),
        vec!["mid"]
    );
}

/// A plain read keeps the text; a string comparison is unchanged.
#[test]
fn plain_and_compare_unchanged() {
    let mut e = setup();
    assert_eq!(
        texts(&mut e, "SELECT e FROM en WHERE e = 'mid'"),
        vec!["mid"]
    );
    // `e < 'high'` is a string compare in MariaDB too (all false here).
    match e
        .execute("SELECT COUNT(*) FROM en WHERE e < 'high'")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], Value::BigInt(0));
        }
        other => panic!("{other:?}"),
    }
}
