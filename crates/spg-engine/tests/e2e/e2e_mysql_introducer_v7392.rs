//! v7.39.2 — MySQL's charset introducers.
//!
//! `SELECT _utf8mb4'x'`, `N'y'`, `_binary'z'` and `_latin1'w'` were each
//! `ERROR 1064 syntax error` here; all four answer the literal on MySQL
//! 9.7.2.
//!
//! This waited for `Expr::Collate`, and the reason is the two rows in
//! `the_comparison_is_what_the_introducer_is_for` below. Accepting the
//! syntax and dropping the charset would have fixed the other four and
//! broken that one in the worse direction: measured, `_binary'A' = 'a'`
//! is **0** on MySQL because `_binary` makes the comparison byte-wise,
//! and a session comparing under its own collation answers **1**. A hard
//! error is honest; a silently wrong comparison is not.
//!
//! Every expectation is MySQL 9.7.2's, measured.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode=''")
        .expect("enter the MySQL dialect");
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) if !rows.is_empty() => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        Ok(_) => "<none>".to_string(),
        Err(err) => panic!("{sql}: {err}"),
    }
}

#[test]
fn every_form_answers_the_literal() {
    let mut e = mysql();
    for (sql, want) in [
        ("SELECT _utf8mb4'x'", "x"),
        // A space between the introducer and the literal is allowed,
        // which falls out of asking the token stream rather than bytes.
        ("SELECT _utf8mb4 'x'", "x"),
        ("SELECT N'y'", "y"),
        ("SELECT n'y'", "y"),
        ("SELECT _binary'z'", "z"),
        ("SELECT _latin1'w'", "w"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

#[test]
fn the_comparison_is_what_the_introducer_is_for() {
    // The pair that made a syntax-only version worse than the error.
    let mut e = mysql();
    assert_eq!(
        one(&mut e, "SELECT _utf8mb4'A' = _utf8mb4'a'"),
        "true",
        "utf8mb4's default collation folds case"
    );
    assert_eq!(
        one(&mut e, "SELECT _binary'A' = 'a'"),
        "false",
        "_binary compares bytes — the row a syntax-only introducer would \
         have answered true"
    );
}

#[test]
fn an_unknown_charset_is_not_an_introducer() {
    // MySQL parses `_nosuch'x'` as a column reference followed by a
    // string and answers `Unknown column '_nosuch'`. What matters here
    // is that SPG does not TREAT it as an introducer — the table
    // decides, and it is the same table `SET NAMES` reads.
    let mut e = mysql();
    let err = e
        .execute("SELECT _nosuch'x'")
        .expect_err("not an introducer");
    let msg = format!("{err}");
    assert!(
        !msg.contains("collation"),
        "it must not have become a collated literal: {msg}"
    );
}

#[test]
fn a_postgresql_session_has_no_introducers() {
    // PG has none, and `_utf8mb4 'x'` there is a column reference
    // followed by a string — a syntax error, not a literal.
    let mut e = Engine::new();
    assert!(e.execute("SELECT _utf8mb4'x'").is_err());
    assert!(e.execute("SELECT N'y'").is_err());
}

#[test]
fn it_works_where_a_literal_works() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT 1 WHERE _utf8mb4'x' = 'x'"), "1");
    e.execute("CREATE TABLE intro (s VARCHAR(8))").unwrap();
    e.execute("INSERT INTO intro VALUES (_utf8mb4'hello')")
        .expect("and as a value");
    assert_eq!(one(&mut e, "SELECT s FROM intro"), "hello");
}
