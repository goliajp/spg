//! v7.39 (round 507) — unary `+`, which SPG did not have.
//!
//! `SELECT +1` worked, so the gap hid: the lexer reads `+1` as one signed
//! literal, and nothing else went through. Every one of these was a syntax
//! error, and PG18 and MariaDB 11 accept all of them:
//!
//!   SELECT + 1        (spaced)
//!   SELECT + + 1      (repeated)
//!   SELECT +a         (a column)
//!   SELECT +(1)       (parenthesised)
//!   SELECT 1 + +1     (which is what templated SQL produces)
//!
//! The two oracles disagree about what it MEANS, so both readings are here.
//! PG's unary plus is the identity on a number and keeps its type, and is
//! refused on anything else — including interval, which unary MINUS
//! accepts. MariaDB's is the identity on everything, with no type check at
//! all. Every expectation below is a reading off one of the two.

use spg_engine::{Engine, QueryResult};

fn pg() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE lbl (a INT, s TEXT)").unwrap();
    e.execute("INSERT INTO lbl VALUES (1, 'x')").unwrap();
    e
}

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e.execute("CREATE TABLE lbl (a INT, s VARCHAR(20))")
        .unwrap();
    e.execute("INSERT INTO lbl VALUES (1, 'x')").unwrap();
    e
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .map(spg_engine::eval::value_to_text)
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

/// The forms that were syntax errors. All of them parse now, on both
/// oracles' evidence.
#[test]
fn round507_unary_plus_parses_in_every_form() {
    let mut e = pg();
    for (sql, want) in [
        ("SELECT +1", "1"),
        ("SELECT + 1", "1"),
        ("SELECT + + 1", "1"),
        ("SELECT + + + 1", "1"),
        ("SELECT +(1)", "1"),
        ("SELECT 1 + +1", "2"),
        ("SELECT +a FROM lbl", "1"),
        ("SELECT +(-1)", "-1"),
    ] {
        assert_eq!(text(&mut e, sql), want, "{sql}");
    }
}

/// PG keeps the operand's type — it is the identity, not a widening.
#[test]
fn round507_pg_unary_plus_keeps_the_operands_type() {
    let mut e = pg();
    for (sql, want) in [
        ("SELECT pg_typeof(+1)", "integer"),
        ("SELECT pg_typeof(+1.5)", "numeric"),
        ("SELECT pg_typeof(+'2'::bigint)", "bigint"),
        ("SELECT pg_typeof(+a) FROM lbl", "integer"),
    ] {
        assert_eq!(text(&mut e, sql), want, "{sql}");
    }
    // NULL in, NULL out. Compared against the bare operand rather than a
    // spelling, so the assertion is about the OPERATOR and not about how
    // this harness happens to render a null.
    assert_eq!(text(&mut e, "SELECT + NULL"), text(&mut e, "SELECT NULL"));
}

/// PG has no unary `+` for non-numeric operands, and says so in the wording
/// it uses for every missing operator. Interval is the surprise: unary MINUS
/// takes one, unary plus does not.
#[test]
fn round507_pg_unary_plus_is_refused_on_non_numbers() {
    let mut e = pg();
    for (sql, want) in [
        ("SELECT + TRUE", "operator does not exist: + boolean"),
        ("SELECT + 'x'::text", "operator does not exist: + text"),
        (
            "SELECT + INTERVAL '1 day'",
            "operator does not exist: + interval",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql}: expected {want:?}, got {got}");
    }
    // Unary minus DOES take an interval — the asymmetry is PG's, not a slip.
    assert_eq!(text(&mut e, "SELECT (- INTERVAL '1 day')::text"), "-1 days");
}

/// MariaDB's unary plus is a pure no-op: it takes anything and returns it,
/// including the operands PG refuses.
///
/// Each case compares `+ x` against bare `x`, which is the identity claim
/// itself. Spelling out an expected string instead would drag in how a
/// value RENDERS — a separate question with its own answer per dialect,
/// and not what this is testing.
#[test]
fn round507_mysql_unary_plus_is_the_identity_on_anything() {
    let mut e = mysql();
    for operand in ["1", "'x'", "TRUE", "NULL", "1.5", "s FROM lbl"] {
        let plain = text(&mut e, &format!("SELECT {operand}"));
        let plused = text(&mut e, &format!("SELECT + {operand}"));
        assert_eq!(plused, plain, "+ {operand}");
    }
    // And it composes with the arithmetic around it.
    assert_eq!(text(&mut e, "SELECT 1 + +1"), "2");
    assert_eq!(text(&mut e, "SELECT + + 1"), "1");
}
