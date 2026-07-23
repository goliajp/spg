//! read01 round 377 (MySQL differential) — CHARSET(x) / COLLATION(x)
//! introspection functions.
//!
//! MariaDB 11: `CHARSET('x')` is 'utf8mb4' and `COLLATION('x')` is
//! 'utf8mb4_uca1400_ai_ci', while a number or a binary string reports
//! 'binary' for both. SPG had neither function, so an ORM or a migration
//! that introspects a column's charset/collation failed outright. SPG
//! stores text as UTF-8 with the folding default collation, so a
//! text-typed argument reports those names; everything else is 'binary'.
//! PG has no such functions — a PostgreSQL session keeps the honest
//! "function does not exist" error.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
        {
            Some(Value::Text(s)) => s.into_owned(),
            other => panic!("`{sql}` not text: {other:?}"),
        },
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// A text argument reports utf8mb4 / the folding default collation.
#[test]
fn text_reports_utf8mb4() {
    let mut e = mysql();
    assert_eq!(text(&mut e, "SELECT CHARSET('x')"), "utf8mb4");
    assert_eq!(
        text(&mut e, "SELECT COLLATION('x')"),
        "utf8mb4_uca1400_ai_ci"
    );
    assert_eq!(text(&mut e, "SELECT CHARSET(CONCAT('a', 'b'))"), "utf8mb4");
}

/// A number or a binary string reports 'binary'.
#[test]
fn number_and_binary_report_binary() {
    let mut e = mysql();
    assert_eq!(text(&mut e, "SELECT CHARSET(123)"), "binary");
    assert_eq!(text(&mut e, "SELECT COLLATION(123)"), "binary");
}

/// The argument's column type decides: VARCHAR is utf8mb4, VARBINARY is
/// binary.
#[test]
fn column_type_decides() {
    let mut e = mysql();
    e.execute("CREATE TABLE ct (v VARCHAR(10), b VARBINARY(10))")
        .unwrap();
    e.execute("INSERT INTO ct VALUES ('x', 'y')").unwrap();
    assert_eq!(text(&mut e, "SELECT CHARSET(v) FROM ct"), "utf8mb4");
    assert_eq!(text(&mut e, "SELECT CHARSET(b) FROM ct"), "binary");
    assert_eq!(
        text(&mut e, "SELECT COLLATION(v) FROM ct"),
        "utf8mb4_uca1400_ai_ci"
    );
    assert_eq!(text(&mut e, "SELECT COLLATION(b) FROM ct"), "binary");
}

/// A PostgreSQL session has no CHARSET / COLLATION function.
#[test]
fn postgres_session_has_no_such_function() {
    let mut p = Engine::new();
    assert!(p.execute("SELECT CHARSET('x')").is_err());
    assert!(p.execute("SELECT COLLATION('x')").is_err());
}
