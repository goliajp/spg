//! read01 round 408 (MySQL differential) — string functions coerce numeric /
//! temporal arguments.
//!
//! MySQL implicitly stringifies a number or date passed to a string function:
//! `LOCATE(2, 12345)` searches "2" in "12345" = 2, `SUBSTRING_INDEX(123.456,
//! '.', 1)` = "123", `FIND_IN_SET(2, '1,2,3')` = 2, `INSTR(12345, 3)` = 3.
//! `ELT` rounds a fractional index (`ELT(1.9, …)` picks the 2nd, `ELT(2.5,
//! …)` the 3rd — half away from zero). `FIELD` compares as strings only when
//! every argument is a string; if any is a number it compares them all as
//! DOUBLE (`FIELD(2, '1', '2')` = 2, `FIELD(2.0, 1, 2, 3)` = 2). SPG rejected
//! every non-text argument. A PostgreSQL session keeps the strict type error.
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
            Value::Int(n) => n.to_string(),
            Value::BigInt(n) => n.to_string(),
            Value::Text(s) => s.to_string(),
            Value::Null => "NULL".to_string(),
            o => panic!("{o:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// LOCATE / INSTR coerce numeric operands to their string form.
#[test]
fn locate_instr_numeric() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT LOCATE(2, 12345)"), "2");
    assert_eq!(scalar(&mut e, "SELECT LOCATE('2', 12345)"), "2");
    assert_eq!(scalar(&mut e, "SELECT INSTR(12345, 3)"), "3");
    // A DATE stringifies to YYYY-MM-DD.
    assert_eq!(scalar(&mut e, "SELECT LOCATE('-', DATE '2020-01-05')"), "5");
}

/// SUBSTRING_INDEX / FIND_IN_SET coerce numeric operands.
#[test]
fn substring_index_find_in_set_numeric() {
    let mut e = mysql();
    assert_eq!(
        scalar(&mut e, "SELECT SUBSTRING_INDEX(123.456, '.', 1)"),
        "123"
    );
    assert_eq!(scalar(&mut e, "SELECT FIND_IN_SET(2, '1,2,3')"), "2");
    assert_eq!(scalar(&mut e, "SELECT FIND_IN_SET('b', 'a,b,c')"), "2");
}

/// ELT rounds a fractional index (half away from zero).
#[test]
fn elt_rounds_index() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT ELT(1.9,'a','b','c')"), "b");
    assert_eq!(scalar(&mut e, "SELECT ELT(2.5,'a','b','c','d')"), "c");
    assert_eq!(scalar(&mut e, "SELECT ELT(3.5,'a','b','c','d')"), "d");
    assert_eq!(scalar(&mut e, "SELECT ELT(0.9,'a','b')"), "a");
    assert_eq!(scalar(&mut e, "SELECT ELT(2,'a','b','c')"), "b");
}

/// FIELD: all-string compares as strings; any number compares all as double.
#[test]
fn field_type_dependent_compare() {
    let mut e = mysql();
    // all strings -> string compare
    assert_eq!(scalar(&mut e, "SELECT FIELD('b','a','b','c')"), "2");
    // numeric -> double compare
    assert_eq!(scalar(&mut e, "SELECT FIELD(2,1,2,3)"), "2");
    assert_eq!(scalar(&mut e, "SELECT FIELD(2.0,1,2,3)"), "2");
    assert_eq!(scalar(&mut e, "SELECT FIELD(2.5,1,2.5,3)"), "2");
    // mixed -> double: 2 vs 'a'(0.0),'2'(2.0),'3'(3.0) -> 2
    assert_eq!(scalar(&mut e, "SELECT FIELD(2,'a','2','3')"), "2");
    // mixed with fractional string: 1.0 == '1.0' at position 1
    assert_eq!(scalar(&mut e, "SELECT FIELD(1.0,'1.0','1','2')"), "1");
    // NULL search value never matches
    assert_eq!(scalar(&mut e, "SELECT FIELD(NULL,1,2)"), "0");
}

/// A PostgreSQL session keeps the strict type error for non-text args,
/// while an all-text FIELD still works.
#[test]
fn postgres_strict() {
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT FIELD(2,1,2,3)").is_err(),
        "PG rejects a numeric FIELD search value"
    );
    assert!(
        e.execute("SELECT LOCATE(2, 12345)").is_err(),
        "PG rejects numeric LOCATE args"
    );
    assert_eq!(scalar(&mut e, "SELECT field('b','a','b','c')"), "2");
}
