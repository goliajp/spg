//! read01 round 422 (MySQL differential) — a non-literal INTERVAL quantity.
//!
//! MySQL's interval quantity may be ANY expression, not just a literal:
//!     DATE_ADD(d, INTERVAL n DAY)        -- a column
//!     d + INTERVAL n*2 DAY               -- an expression
//!     INTERVAL (1+1) DAY                 -- parenthesised
//!     DATE_ADD(d, INTERVAL ABS(-5) DAY)  -- a function result
//! SPG only accepted a literal number, so every one of these was a parse
//! error — and MySQL writes essentially all of its date arithmetic this way.
//!
//! A non-constant quantity cannot fold into a compile-time
//! `Literal::Interval`, so it lowers onto the existing
//! `make_interval(years, months, weeks, days, hours, mins, secs)` builtin,
//! which constructs the value at run time (NULL quantity -> NULL, as
//! MariaDB). The literal path still folds the constant case.
//!
//! `INTERVAL (` stays ambiguous with round 409's `INTERVAL(N, N1, …)`
//! function; the shape is now decided by a NON-DESTRUCTIVE lookahead over
//! the token indices. (Round 409 parsed the group and restored `self.pos`,
//! which could never work — `advance()` replaces the token it returns with
//! Eof. It was inert only because both branches errored back then.)
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn texts(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                Value::Null => "NULL".to_string(),
                v => spg_engine::eval::value_to_text(v),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn one(e: &mut Engine, sql: &str) -> String {
    texts(e, sql).remove(0)
}

fn seed() -> Engine {
    let mut e = mysql();
    e.execute("CREATE TABLE t(n INT, d DATE)").unwrap();
    e.execute("INSERT INTO t VALUES(3,'2020-01-01'),(10,'2020-06-15')")
        .unwrap();
    e
}

/// A parenthesised quantity — the shape that collides with INTERVAL().
#[test]
fn parenthesised_quantity() {
    let mut e = mysql();
    assert_eq!(
        one(&mut e, "SELECT DATE_ADD('2020-01-01', INTERVAL (1+1) DAY)"),
        "2020-01-03"
    );
    assert_eq!(
        one(&mut e, "SELECT DATE_ADD('2020-01-01 00:00:00', INTERVAL (2*3) HOUR)"),
        "2020-01-01 06:00:00"
    );
}

/// A column quantity, through DATE_ADD / DATE_SUB and the +/- operators.
#[test]
fn column_quantity() {
    let mut e = seed();
    assert_eq!(
        texts(&mut e, "SELECT DATE_ADD(d, INTERVAL n DAY) FROM t ORDER BY n"),
        vec!["2020-01-04", "2020-06-25"]
    );
    assert_eq!(
        texts(&mut e, "SELECT DATE_SUB(d, INTERVAL n DAY) FROM t ORDER BY n"),
        vec!["2019-12-29", "2020-06-05"]
    );
    assert_eq!(
        texts(&mut e, "SELECT d + INTERVAL n DAY FROM t ORDER BY n"),
        vec!["2020-01-04", "2020-06-25"]
    );
    assert_eq!(
        texts(&mut e, "SELECT d - INTERVAL n DAY FROM t ORDER BY n"),
        vec!["2019-12-29", "2020-06-05"]
    );
}

/// An arithmetic expression and a function call as the quantity.
#[test]
fn expression_quantity() {
    let mut e = seed();
    assert_eq!(
        texts(&mut e, "SELECT DATE_ADD(d, INTERVAL n*2 DAY) FROM t ORDER BY n"),
        vec!["2020-01-07", "2020-07-05"]
    );
    assert_eq!(
        one(&mut e, "SELECT DATE_ADD('2020-01-01', INTERVAL ABS(-5) DAY)"),
        "2020-01-06"
    );
    assert_eq!(
        one(&mut e, "SELECT DATE_ADD('2020-01-10', INTERVAL -3 DAY)"),
        "2020-01-07"
    );
}

/// Every unit routes to the right slot of make_interval.
#[test]
fn every_unit() {
    let mut e = mysql();
    e.execute("CREATE TABLE u(n INT)").unwrap();
    e.execute("INSERT INTO u VALUES(2)").unwrap();
    let d = |u: &str| format!("SELECT DATE_ADD('2020-01-01', INTERVAL n {u}) FROM u");
    let ts = |u: &str| format!("SELECT DATE_ADD('2020-01-01 00:00:00', INTERVAL n {u}) FROM u");
    assert_eq!(one(&mut e, &d("YEAR")), "2022-01-01");
    assert_eq!(one(&mut e, &d("QUARTER")), "2020-07-01");
    assert_eq!(one(&mut e, &d("MONTH")), "2020-03-01");
    assert_eq!(one(&mut e, &d("WEEK")), "2020-01-15");
    assert_eq!(one(&mut e, &d("DAY")), "2020-01-03");
    assert_eq!(one(&mut e, &ts("HOUR")), "2020-01-01 02:00:00");
    assert_eq!(one(&mut e, &ts("MINUTE")), "2020-01-01 00:02:00");
    assert_eq!(one(&mut e, &ts("SECOND")), "2020-01-01 00:00:02");
}

/// MICROSECOND rides the fractional-seconds slot. NOTE: SPG renders the
/// fraction PG-style (trailing zeros trimmed) where MariaDB pads to six —
/// the instant is identical; the padding is a general timestamp-render
/// property, tracked separately.
#[test]
fn microsecond_unit_value_is_exact() {
    let mut e = mysql();
    e.execute("CREATE TABLE u(n INT)").unwrap();
    e.execute("INSERT INTO u VALUES(1500000),(250000)").unwrap();
    assert_eq!(
        texts(
            &mut e,
            "SELECT DATE_ADD('2020-01-01 00:00:00', INTERVAL n MICROSECOND) FROM u ORDER BY n"
        ),
        vec!["2020-01-01 00:00:00.25", "2020-01-01 00:00:01.5"]
    );
}

/// A NULL quantity yields NULL.
#[test]
fn null_quantity() {
    let mut e = mysql();
    assert_eq!(
        one(&mut e, "SELECT DATE_ADD('2020-01-01', INTERVAL NULL DAY)"),
        "NULL"
    );
}

/// The literal path and round 409's `INTERVAL(N, …)` function both still
/// work — the lookahead has to keep them apart.
#[test]
fn literal_and_function_forms_unchanged() {
    let mut e = mysql();
    assert_eq!(
        one(&mut e, "SELECT DATE_ADD('2020-01-01', INTERVAL 1 DAY)"),
        "2020-01-02"
    );
    assert_eq!(one(&mut e, "SELECT INTERVAL(5,1,3,7)"), "2");
    assert_eq!(one(&mut e, "SELECT INTERVAL(0,1,3,7)"), "0");
}

/// A PostgreSQL session keeps its interval literals and still rejects the
/// MySQL unquoted-quantity form.
#[test]
fn postgres_unchanged() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT DATE '2020-01-01' + INTERVAL '3 days'"), "2020-01-04 00:00:00");
    assert_eq!(one(&mut e, "SELECT INTERVAL '1' DAY"), "1 day");
    assert_eq!(one(&mut e, "SELECT INTERVAL '2 hours'"), "02:00:00");
    e.execute("CREATE TABLE t(n INT)").unwrap();
    e.execute("INSERT INTO t VALUES(3)").unwrap();
    assert!(
        e.execute("SELECT DATE '2020-01-01' + INTERVAL n DAY FROM t").is_err(),
        "PG has no unquoted INTERVAL quantity"
    );
}
