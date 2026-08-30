//! v7.39.3 — two spellings of a column name are one column, on MySQL.
//!
//! v7.39.1 closed this for RELATION names and left it open for columns.
//! MySQL's column names are case-INSENSITIVE (measured on 9.7.2:
//! `mycol`, `MYCOL` and a backquoted `MyCol` all resolve a column
//! declared `MyCol`), while SPG compared them byte for byte — and its
//! lexer folds an UNQUOTED identifier.
//!
//! The shape that breaks is a `mysqldump` restore, because mysqldump
//! backquotes every identifier, so the dumped DDL keeps its case while
//! the application's ordinary SQL does not:
//!
//!     CREATE TABLE `Orders` (`OrderID` INT)   -- from the dump
//!     SELECT `OrderID` FROM `Orders`          -- worked
//!     SELECT OrderID FROM Orders              -- 1054, unknown column
//!
//! PostgreSQL's rule is the opposite and is untouched: a quoted
//! identifier is case-SENSITIVE there, and folding it would break every
//! column that relies on it.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) if !rows.is_empty() => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        Ok(other) => panic!("{sql}: {other:?}"),
        Err(err) => panic!("{sql}: {err}"),
    }
}

#[test]
fn a_dumped_table_is_reachable_from_ordinary_sql() {
    let mut e = mysql();
    // Exactly what mysqldump writes: every identifier backquoted.
    e.execute("CREATE TABLE `Orders` (`OrderID` INT, `CustomerName` VARCHAR(40))")
        .expect("ddl");
    e.execute("INSERT INTO `Orders` (`OrderID`, `CustomerName`) VALUES (1, 'a')")
        .expect("insert");
    // The dump's own spelling.
    assert_eq!(one(&mut e, "SELECT `OrderID` FROM `Orders`"), "1");
    // And the application's, which is the one that used to fail.
    assert_eq!(one(&mut e, "SELECT OrderID FROM Orders"), "1");
    assert_eq!(one(&mut e, "SELECT orderid FROM Orders"), "1");
    assert_eq!(one(&mut e, "SELECT ORDERID FROM Orders"), "1");
    // In a predicate and through a qualifier too.
    assert_eq!(
        one(&mut e, "SELECT customername FROM Orders WHERE OrderID = 1"),
        "a"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT o.CUSTOMERNAME FROM Orders o WHERE o.orderid = 1"
        ),
        "a"
    );
}

#[test]
fn an_unknown_column_is_still_unknown() {
    let mut e = mysql();
    e.execute("CREATE TABLE `C` (`Col` INT)").expect("ddl");
    // Case-insensitive is not "anything goes": a name no column bears
    // is still refused, and the sentence names the clause.
    // The PROJECTION path raises the engine's canonical sentence and
    // the MySQL wire words it; the clause validator below words it
    // in-engine, which is why the two assertions differ.
    let err = format!("{}", e.execute("SELECT nosuch FROM C").unwrap_err());
    assert!(err.contains("column \"nosuch\" does not exist"), "{err}");
    let err = format!(
        "{}",
        e.execute("SELECT Col FROM C WHERE nope = 1").unwrap_err()
    );
    assert!(err.contains("in 'where clause'"), "{err}");
}

/// The control: PostgreSQL's quoted identifiers stay case-SENSITIVE.
/// Folding them would break every column that relies on the quotes.
#[test]
fn a_postgres_session_keeps_its_quoted_case() {
    let mut e = Engine::new();
    e.execute(r#"CREATE TABLE p ("MyCol" INT, plain INT)"#)
        .expect("ddl");
    e.execute("INSERT INTO p VALUES (1, 2)").expect("insert");
    assert_eq!(one(&mut e, r#"SELECT "MyCol" FROM p"#), "1");
    // Unquoted folds to lowercase, which is a DIFFERENT name there.
    assert!(e.execute("SELECT MyCol FROM p").is_err());
    assert!(e.execute(r#"SELECT "mycol" FROM p"#).is_err());
    // And an unquoted declaration is reachable however it is written,
    // because both sides fold.
    assert_eq!(one(&mut e, "SELECT PLAIN FROM p"), "2");
}
