//! v7.39 — which quote opens a string, and who decides.
//!
//! Two axes, one flag. `backslash_escapes` meant both "this session
//! speaks MySQL" and "backslashes escape", and `"…"` needs the first
//! while `SET sql_mode='NO_BACKSLASH_ESCAPES'` turns off the second.
//! The engine-side truth table, so a change to either axis has to say
//! what it does to the other.
//!
//! Values read off MySQL 9.7.2 and PostgreSQL 18.6.

use spg_engine::{Engine, QueryResult};

fn ask(e: &mut Engine, sql: &str) -> Result<String, String> {
    match e.execute(sql) {
        Err(err) => Err(format!("{err}")),
        Ok(QueryResult::Rows { rows, .. }) if !rows.is_empty() => {
            Ok(spg_engine::eval::value_to_text(&rows[0].values[0]))
        }
        Ok(_) => Ok("<ok>".to_string()),
    }
}

#[test]
fn a_postgresql_session_reads_a_double_quote_as_an_identifier() {
    // The default, and it must not move: every PG client depends on it.
    let mut e = Engine::new();
    assert!(
        ask(&mut e, r#"SELECT "abc""#)
            .unwrap_err()
            .contains(r#"column "abc" does not exist"#),
        "PG names an identifier here"
    );
    assert_eq!(ask(&mut e, r#"SELECT 1 AS "x""#).unwrap(), "1");
    e.execute(r#"CREATE TABLE q ("Mixed" int)"#).unwrap();
    e.execute("INSERT INTO q VALUES (7)").unwrap();
    assert_eq!(
        ask(&mut e, r#"SELECT "Mixed" FROM q"#).unwrap(),
        "7",
        "a quoted mixed-case column keeps its case, which is why PG has the rule"
    );
}

#[test]
fn the_dialect_truth_table() {
    // (sql_mode, what "abc" is)
    let mut e = Engine::new();
    // A session becomes MySQL by sending sql_mode — only a MySQL client
    // or a mysqldump preamble does.
    e.execute("SET sql_mode=''").unwrap();
    assert_eq!(
        ask(&mut e, r#"SELECT "abc""#).unwrap(),
        "abc",
        "MySQL: string"
    );

    e.execute("SET sql_mode='NO_BACKSLASH_ESCAPES'").unwrap();
    assert_eq!(
        ask(&mut e, r#"SELECT "abc""#).unwrap(),
        "abc",
        "turning escapes off does not make the session any less MySQL"
    );

    e.execute("SET sql_mode='ANSI_QUOTES'").unwrap();
    assert!(
        ask(&mut e, r#"SELECT "abc""#).unwrap_err().contains("abc"),
        "ANSI_QUOTES: identifier"
    );

    e.execute("SET sql_mode='ANSI_QUOTES,NO_BACKSLASH_ESCAPES'")
        .unwrap();
    assert!(
        ask(&mut e, r#"SELECT "abc""#).unwrap_err().contains("abc"),
        "both flags together: still an identifier"
    );

    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    assert_eq!(
        ask(&mut e, r#"SELECT "abc""#).unwrap(),
        "abc",
        "sql_mode is a whole replacement, so ANSI_QUOTES is gone again"
    );
}

#[test]
fn escapes_and_quoting_move_independently() {
    let mut e = Engine::new();
    e.execute("SET sql_mode=''").unwrap();
    assert_eq!(ask(&mut e, r#"SELECT LENGTH("\n")"#).unwrap(), "1");
    assert_eq!(ask(&mut e, r"SELECT LENGTH('\n')").unwrap(), "1");
    e.execute("SET sql_mode='NO_BACKSLASH_ESCAPES'").unwrap();
    assert_eq!(ask(&mut e, r#"SELECT LENGTH("\n")"#).unwrap(), "2");
    assert_eq!(ask(&mut e, r"SELECT LENGTH('\n')").unwrap(), "2");
}

#[test]
fn a_mysql_double_quoted_string_follows_mysqls_own_rules() {
    let mut e = Engine::new();
    e.execute("SET sql_mode=''").unwrap();
    assert_eq!(ask(&mut e, r#"SELECT "a""b""#).unwrap(), "a\"b");
    assert_eq!(ask(&mut e, r#"SELECT "a\"b""#).unwrap(), "a\"b");
    assert_eq!(ask(&mut e, r#"SELECT "a'b""#).unwrap(), "a'b");
    // A single-quoted string keeps its own doubling rule alongside.
    assert_eq!(ask(&mut e, "SELECT 'a''b'").unwrap(), "a'b");
}
