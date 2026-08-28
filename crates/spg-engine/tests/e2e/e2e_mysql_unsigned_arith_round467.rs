//! read01 round 467 (MySQL differential) — UNSIGNED arithmetic range check.
//!
//! `INT UNSIGNED` columns holding 1 and 5 made `a - b` answer **-4** in a
//! MySQL session. MariaDB 11 raises `ERROR 1690 (22003): BIGINT UNSIGNED
//! value is out of range`. A negative answer from a column the server
//! promises is non-negative is the kind of value an application stores back
//! into the same column, so it was silent and wrong in the worst direction.
//!
//! Every expectation below is copied from a MariaDB 11 run. The rule it
//! encodes: MySQL decides unsignedness STATICALLY, from the expression's
//! type. `SUM(a) - 100` answers -99 on the same column because SUM's result
//! type is not unsigned; unary minus does not propagate it either.

use spg_engine::{Engine, QueryResult};

fn mysql_engine() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_wire_session();
    e.execute("CREATE TABLE u (a INT UNSIGNED, b INT UNSIGNED, c BIGINT UNSIGNED, t TINYINT UNSIGNED, s SMALLINT UNSIGNED)").unwrap();
    e.execute("INSERT INTO u VALUES (1, 5, 1, 1, 1)").unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> Result<String, String> {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => {
            Ok(spg_engine::eval::value_to_text(&rows[0].values[0]))
        }
        Ok(other) => Err(alloc_fmt(&other)),
        Err(e) => Err(format!("{e}")),
    }
}

fn alloc_fmt(q: &QueryResult) -> String {
    format!("{q:?}")
}

/// Shapes MariaDB 11 raises 1690 on.
const RAISES: &[&str] = &[
    "SELECT a - b FROM u",
    "SELECT t - 5 FROM u",
    "SELECT s - 5 FROM u",
    "SELECT c - 5 FROM u",
    "SELECT a - 5 FROM u",
    "SELECT 1 - b FROM u",
    "SELECT 0 - a FROM u",
    "SELECT CAST(1 AS UNSIGNED) - 5",
    "SELECT a * 0 - 1 FROM u",
    // The error travels: an enclosing function does not swallow it.
    "SELECT ABS(a - b) FROM u",
    "SELECT a - b + 10 FROM u",
];

/// Shapes MariaDB 11 answers, with the answer it gives.
const ANSWERS: &[(&str, &str)] = &[
    ("SELECT b - a FROM u", "4"),
    ("SELECT 5 - a FROM u", "4"),
    ("SELECT CAST(5 AS UNSIGNED) - CAST(1 AS UNSIGNED)", "4"),
    ("SELECT a + b FROM u", "6"),
    // SUM's result type is not unsigned, so the subtraction is signed.
    ("SELECT SUM(a) - 100 FROM u", "-99"),
    // Unary minus does not propagate unsignedness in MariaDB.
    ("SELECT -CAST(1 AS UNSIGNED)", "-1"),
    // CAST wraps rather than raising.
    ("SELECT CAST(-1 AS UNSIGNED)", "18446744073709551615"),
];

#[test]
fn round467_unsigned_underflow_raises_as_mariadb_does() {
    let mut e = mysql_engine();
    for sql in RAISES {
        let got = one(&mut e, sql);
        let Err(msg) = got else {
            panic!("`{sql}` answered {got:?}; MariaDB raises 1690");
        };
        assert!(
            msg.contains("BIGINT UNSIGNED value is out of range"),
            "`{sql}` failed with the wrong error: {msg}"
        );
    }
}

#[test]
fn round467_in_range_unsigned_arithmetic_still_answers() {
    let mut e = mysql_engine();
    for (sql, want) in ANSWERS {
        assert_eq!(one(&mut e, sql).as_deref(), Ok(*want), "for `{sql}`");
    }
}

#[test]
fn round467_the_message_names_the_expression_as_mariadb_does() {
    // MariaDB writes the offending expression with minimal parentheses:
    // `a * 0 - 1`, not `((a * 0) - 1)`. (It also fully qualifies its column
    // names in backticks, which the evaluation context cannot reach — a
    // recorded difference, not a matched one.)
    let mut e = mysql_engine();
    let Err(msg) = one(&mut e, "SELECT a * 0 - 1 FROM u") else {
        panic!("expected a range error");
    };
    assert!(msg.contains("in 'a * 0 - 1'"), "message was: {msg}");
}

#[test]
fn round467_postgres_sessions_are_untouched() {
    // PG has no UNSIGNED, and the guard is dialect-gated: a PG session must
    // keep answering whatever it answered before.
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (a INT, b INT)").unwrap();
    e.execute("INSERT INTO p VALUES (1, 5)").unwrap();
    assert_eq!(one(&mut e, "SELECT a - b FROM p").as_deref(), Ok("-4"));
}
