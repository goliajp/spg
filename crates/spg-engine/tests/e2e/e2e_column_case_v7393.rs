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

/// v7.39.3 — the SPELLING a column was declared with is kept.
///
/// MySQL 9.7.2 reports the declared case in `SHOW COLUMNS`,
/// `information_schema` and `SHOW CREATE TABLE` whether or not the name
/// was backquoted. SPG's lexer folds an unquoted identifier, so
/// `CREATE TABLE t (MyCol INT)` reported `mycol` — and an ORM comparing
/// its model against the catalog saw a difference on every column, on
/// every run, for ever.
///
/// The lexer folds globally, so the written form comes back from the
/// SOURCE, which the parser keeps in the MySQL dialect. Lookup is
/// case-insensitive there, so storing the written case costs nothing at
/// read time — the test above is what says so.
#[test]
fn the_declared_spelling_is_kept() {
    let mut e = mysql();
    e.execute("CREATE TABLE idc (MyCol INT, other_col INT, `BackTicked` INT)")
        .expect("ddl");
    let names = match e.execute(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_name = 'idc' ORDER BY ordinal_position",
    ) {
        Ok(QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(names, ["MyCol", "other_col", "BackTicked"]);
    // And it is still reachable however it is written.
    e.execute("INSERT INTO idc VALUES (1, 2, 3)")
        .expect("insert");
    assert_eq!(one(&mut e, "SELECT mycol FROM idc"), "1");
    assert_eq!(one(&mut e, "SELECT BACKTICKED FROM idc"), "3");
}

/// The consequence worth stating: a table created on the MySQL wire now
/// carries mixed-case column names, and PostgreSQL's rule for those is
/// that they need quotes. That is PostgreSQL being PostgreSQL about a
/// column genuinely named `MyCol` — the same as for a backquoted
/// declaration, which has stored its case since long before this — but
/// it is a change for an unquoted MySQL declaration, so it is pinned
/// rather than left to be discovered.
#[test]
fn a_postgres_session_sees_the_kept_case_and_says_so() {
    let mut e = mysql();
    e.execute("CREATE TABLE x (MyCol INT)").expect("ddl");
    e.execute("INSERT INTO x VALUES (7)").expect("insert");
    let mut pg = e;
    pg.set_mysql_dialect(false);
    assert_eq!(one(&mut pg, r#"SELECT "MyCol" FROM x"#), "7");
    // Unquoted folds, and `mycol` is not this column's name.
    assert!(pg.execute("SELECT mycol FROM x").is_err());
}

/// The guard on the recovered spelling, which had no pin until an
/// ablation said so.
///
/// The written form comes back as the SOURCE SPAN between this token
/// and the next one, so anything sitting between them comes back with
/// it — a comment, or unusual spacing. Without the check that the
/// recovered text is the same name, `CREATE TABLE t (MyCol /* c */ INT)`
/// names the column `MyCol /* c */`. With it, the span is rejected and
/// the folded name stands, which is the safe direction.
#[test]
fn a_span_that_is_not_the_name_is_not_used() {
    let mut e = mysql();
    e.execute("CREATE TABLE cmt (MyCol /* between */ INT, Plain    INT)")
        .expect("ddl");
    let names = match e.execute(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_name = 'cmt' ORDER BY ordinal_position",
    ) {
        Ok(QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    // The commented one falls back to the folded name rather than
    // carrying the comment into the catalog; the spaced one keeps its
    // case, because trimming leaves the name itself.
    assert_eq!(names, ["mycol", "Plain"]);
    // And both are still reachable.
    e.execute("INSERT INTO cmt VALUES (1, 2)").expect("insert");
    assert_eq!(one(&mut e, "SELECT MyCol, plain FROM cmt"), "1");
}
