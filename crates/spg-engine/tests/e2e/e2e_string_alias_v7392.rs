//! v7.39.2 — a string can name a projection item, and two of them join.
//!
//! `SELECT 1 'x'` is `syntax error at or near "'x'"` here and names the
//! column `x` on MySQL 9.7.2. So do `SELECT 1 AS 'x'`, `SELECT 1 "x"`
//! and `SELECT COUNT(*) 'total'` — ordinary MySQL SQL that would not
//! parse.
//!
//! The two rules had to move together. MySQL joins adjacent string
//! literals with NO line break between them (`SELECT 'a' 'b'` is `ab`
//! there and a syntax error on PostgreSQL 18.6, which joins them only
//! across one), and SPG required the break on both wires. Without that
//! fixed first, `'a' 'b'` would read as a literal ALIASED `b`.
//!
//! A string names a projection item and NOT a table: `FROM t 'ta'` and
//! `FROM t AS 'ta'` are both syntax errors on MySQL 9.7.2, measured.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    e
}

fn label_and_value(e: &mut Engine, sql: &str) -> (String, String) {
    match e.execute(sql) {
        Ok(QueryResult::Rows { columns, rows }) if !rows.is_empty() => (
            columns[0].name.clone(),
            spg_engine::eval::value_to_text(&rows[0].values[0]),
        ),
        Ok(_) => ("<none>".into(), "<none>".into()),
        Err(err) => ("ERR".into(), err.to_string()),
    }
}

#[test]
fn a_string_names_a_projection_item() {
    let mut e = mysql();
    for sql in [
        "SELECT 1 'x'",
        "SELECT 1 AS 'x'",
        // In MySQL's default sql_mode `"…"` is a STRING, so this is the
        // same rule reached through the other quote.
        "SELECT 1 \"x\"",
    ] {
        assert_eq!(
            label_and_value(&mut e, sql),
            ("x".into(), "1".into()),
            "{sql}"
        );
    }
    // A space is why one quotes it at all.
    assert_eq!(
        label_and_value(&mut e, "SELECT 1 'a b'"),
        ("a b".into(), "1".into())
    );
    // Two items, so the rule cannot be swallowing the comma.
    match e.execute("SELECT 1 'x', 2 'y'") {
        Ok(QueryResult::Rows { columns, .. }) => {
            assert_eq!(columns[0].name, "x");
            assert_eq!(columns[1].name, "y");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn adjacent_literals_join_without_a_line_break() {
    let mut e = mysql();
    // MySQL 9.7.2: `ab`. This has to be decided BEFORE the alias rule,
    // or the second literal would look like a name.
    assert_eq!(label_and_value(&mut e, "SELECT 'a' 'b'").1, "ab");
    assert_eq!(label_and_value(&mut e, "SELECT 'a'\n'b'").1, "ab");
}

#[test]
fn a_string_does_not_name_a_table() {
    // Measured on MySQL 9.7.2: both of these are syntax errors, so the
    // rule belongs to the projection and not to `parse_optional_alias`,
    // which names tables too.
    let mut e = mysql();
    e.execute("CREATE TABLE al (c INT)").expect("create");
    for sql in ["SELECT * FROM al 'ta'", "SELECT * FROM al AS 'ta'"] {
        assert!(
            e.execute(sql).is_err(),
            "{sql} must not parse: MySQL refuses it"
        );
    }
}

#[test]
fn postgres_refuses_both() {
    // The negative control, measured on PostgreSQL 18.6: a string is not
    // an alias there, and two literals join only across a line break.
    let mut e = Engine::new();
    assert!(e.execute("SELECT 1 'x'").is_err(), "PG has no string alias");
    assert!(
        e.execute("SELECT 'a' 'b'").is_err(),
        "PG needs the line break"
    );
    assert_eq!(label_and_value(&mut e, "SELECT 'a'\n'b'").1, "ab");
}
