//! read01 round 346 (MySQL differential, M1) — a predicate is a truth value.
//!
//! The engine wrote `matches!(v, Value::Bool(true))` at every position that
//! wants a truth value — WHERE, CASE WHEN, ON, HAVING — so anything that
//! was not ALREADY a boolean silently read as FALSE. Two consequences, both
//! silent:
//!
//!   * `SELECT … WHERE 1` returned **no rows at all**. MariaDB returns
//!     every row; PG raises `argument of WHERE must be type boolean`.
//!   * `CASE WHEN 1 THEN 'a' END` answered NULL in both dialects, and so
//!     did `CASE WHEN 'true' …` — which PG accepts, because it resolves a
//!     bare literal there through boolean input.
//!
//! Measured on MariaDB 11: any non-zero number is true (`-1` and `0.5`
//! included), NULL is not, and a string contributes its LEADING number, so
//! `'1abc'` is true while `'abc'` and `''` are false. Measured on PG 18.4:
//! `argument of {WHERE,CASE/WHEN,AND,NOT} must be type boolean, not type
//! integer`, and `invalid input syntax for type boolean: "abc"`.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn fixture(mysql: bool) -> Engine {
    let mut e = Engine::new();
    if mysql {
        e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    }
    e.execute("CREATE TABLE w1 (a INT, b VARCHAR(10))").unwrap();
    e.execute("INSERT INTO w1 VALUES (1,'x'),(0,'y'),(2,'z'),(NULL,'n')")
        .unwrap();
    e
}

fn col(e: &mut Engine, sql: &str) -> Vec<Value<'static>> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .filter_map(|r| r.values.first().cloned().map(Value::into_owned))
            .collect(),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}"),
    }
}

/// The worst of it: a WHERE that should keep every row kept none.
#[test]
fn mysql_where_reads_a_number_as_a_truth_value() {
    let mut e = fixture(true);
    assert_eq!(col(&mut e, "SELECT b FROM w1 WHERE 1").len(), 4);
    assert!(col(&mut e, "SELECT b FROM w1 WHERE 0").is_empty());
    // A column: non-zero passes, zero and NULL do not.
    assert_eq!(
        col(&mut e, "SELECT b FROM w1 WHERE a"),
        vec![Value::text("x"), Value::text("z")],
    );
}

/// MariaDB's exact readings, one per row of the measurement.
#[test]
fn mysql_case_when_matches_mariadb() {
    let mut e = fixture(true);
    for (sql, want) in [
        ("SELECT CASE WHEN 1 THEN 'a' END", "a"),
        ("SELECT CASE WHEN 0 THEN 'a' ELSE 'b' END", "b"),
        ("SELECT CASE WHEN -1 THEN 'a' ELSE 'b' END", "a"),
        ("SELECT CASE WHEN 0.0 THEN 'a' ELSE 'b' END", "b"),
        ("SELECT CASE WHEN 0.5 THEN 'a' ELSE 'b' END", "a"),
        ("SELECT CASE WHEN NULL THEN 'a' ELSE 'b' END", "b"),
        ("SELECT CASE WHEN 'abc' THEN 'a' ELSE 'b' END", "b"),
        ("SELECT CASE WHEN '1abc' THEN 'a' ELSE 'b' END", "a"),
        ("SELECT CASE WHEN '' THEN 'a' ELSE 'b' END", "b"),
    ] {
        assert_eq!(col(&mut e, sql), vec![Value::text(want)], "for `{sql}`");
    }
    // Per row, over a column.
    assert_eq!(
        col(&mut e, "SELECT CASE WHEN a THEN 'T' ELSE 'F' END FROM w1"),
        ["T", "F", "T", "F"].map(Value::text).to_vec(),
    );
}

/// The connectives read the same way.
#[test]
fn mysql_and_or_not_read_truth_values() {
    let mut e = fixture(true);
    assert_eq!(col(&mut e, "SELECT 1 AND 2"), vec![Value::Bool(true)]);
    assert_eq!(col(&mut e, "SELECT 1 AND 0"), vec![Value::Bool(false)]);
    assert_eq!(col(&mut e, "SELECT 0 OR 3"), vec![Value::Bool(true)]);
    assert_eq!(col(&mut e, "SELECT NOT 5"), vec![Value::Bool(false)]);
    assert_eq!(
        col(&mut e, "SELECT b FROM w1 WHERE a AND 1"),
        vec![Value::text("x"), Value::text("z")],
    );
}

/// The PG dialect refuses the same shapes, in PG's own words — it used to
/// answer NULL / no rows just as silently.
#[test]
fn pg_refuses_a_non_boolean_predicate() {
    let mut e = fixture(false);
    assert_eq!(
        err(&mut e, "SELECT CASE WHEN 1 THEN 'a' END"),
        "eval: type mismatch: argument of CASE/WHEN must be type boolean, not type integer",
    );
    assert_eq!(
        err(&mut e, "SELECT b FROM w1 WHERE 1"),
        "eval: type mismatch: argument of WHERE must be type boolean, not type integer",
    );
    assert_eq!(
        err(&mut e, "SELECT b FROM w1 WHERE a"),
        "eval: type mismatch: argument of WHERE must be type boolean, not type integer",
    );
    assert_eq!(
        err(&mut e, "SELECT NOT 5"),
        "eval: type mismatch: argument of NOT must be type boolean, not type integer",
    );
    assert_eq!(
        err(&mut e, "SELECT 1 AND 2"),
        "eval: type mismatch: argument of AND must be type boolean, not type integer",
    );
}

/// PG resolves a bare literal in that position through boolean input, so
/// `'true'` is legal there and `'abc'` is not. Both used to answer FALSE.
#[test]
fn pg_reads_a_literal_through_boolean_input() {
    let mut e = fixture(false);
    assert_eq!(
        col(&mut e, "SELECT CASE WHEN 'true' THEN 'a' ELSE 'b' END"),
        vec![Value::text("a")],
    );
    assert_eq!(
        err(&mut e, "SELECT CASE WHEN 'abc' THEN 'a' ELSE 'b' END"),
        "eval: type mismatch: invalid input syntax for type boolean: \"abc\"",
    );
    // NULL is not an error in either dialect — three-valued logic.
    assert_eq!(
        col(&mut e, "SELECT CASE WHEN NULL THEN 'a' ELSE 'b' END"),
        vec![Value::text("b")],
    );
}
