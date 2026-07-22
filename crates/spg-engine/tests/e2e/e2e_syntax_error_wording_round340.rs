//! read01 round 340 (V56) — PG has two syntax-error wordings, not dozens.
//!
//! PG 18.4 says exactly `syntax error at or near "<token>"` or
//! `syntax error at end of input`, and the quoted token is the lexeme
//! **as the user wrote it**. SPG wrote its own prose per call site:
//! `expected identifier, got Eof`, `unexpected token From in
//! expression`, and — worst — `expected end of input, got Ident("with")`,
//! which put a Rust Debug rendering of the parser's internal token enum
//! in front of a client.
//!
//! Two defects surfaced while measuring, both silent:
//!   * the token index a message pointed at was `pos - 1`, but `advance()`
//!     parks on the final Eof, so `SELECT * FROM` named `FROM` where PG
//!     says end of input;
//!   * `SELECT 1 AS` **parsed clean** and dropped the alias, because the
//!     alias parser returned None on a dangling AS and left the error to
//!     "the next expectation" — of which there is none at end of input.
//!
//! Every wording below is copied from the PG 18.4 run, not composed.

use spg_engine::Engine;

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT)").unwrap();
    e
}

/// Truncated input is `at end of input` — never a token name.
#[test]
fn a_truncated_statement_ends_at_end_of_input() {
    let mut e = fixture();
    for sql in [
        "SELECT * FROM",
        "SELECT * FROM t WHERE",
        "SELECT 1 +",
        "INSERT INTO t VALUES",
        "UPDATE t SET",
        "SELECT * FROM t ORDER",
        "SELECT ((1)",
        "SELECT * FROM t WHERE a =",
        "GRANT",
    ] {
        assert_eq!(
            err(&mut e, sql),
            "parse: syntax error at end of input",
            "for `{sql}`"
        );
    }
}

/// A token that cannot continue the parse is named — in the source's own
/// spelling, lower-case `frm` included.
#[test]
fn an_offending_token_is_named_as_written() {
    let mut e = fixture();
    for (sql, tok) in [
        ("SELECT * frm t", "frm"),
        ("CREATE TABLE t2 (a int) WITH", "WITH"),
        ("DELETE t", "t"),
        ("CREATE TABLE (", "("),
    ] {
        assert_eq!(
            err(&mut e, sql),
            format!("parse: syntax error at or near \"{tok}\""),
            "for `{sql}`"
        );
    }
}

/// A dangling AS was accepted, and the alias silently vanished.
#[test]
fn a_dangling_as_is_rejected() {
    let mut e = fixture();
    assert_eq!(
        err(&mut e, "SELECT 1 AS"),
        "parse: syntax error at end of input"
    );
    // The well-formed alias still parses, of course.
    assert!(e.execute("SELECT 1 AS one").is_ok());
    assert!(e.execute("SELECT a AS b FROM t").is_ok());
}

/// Lexer-level failures are PG's wording too — they used to report a byte
/// offset, which is SPG's internal bookkeeping, not an answer.
#[test]
fn an_unterminated_literal_reads_like_pg() {
    let mut e = fixture();
    assert_eq!(
        err(&mut e, "SELECT 'abc"),
        "parse: unterminated quoted string at or near \"'abc\"",
    );
    assert_eq!(
        err(&mut e, "SELECT \"abc"),
        "parse: unterminated quoted identifier at or near \"\"abc\"",
    );
    assert_eq!(
        err(&mut e, "SELECT 1 /* x"),
        "parse: unterminated /* comment at or near \"/* x\"",
    );
}

/// The messages PG words itself — its own errors, not its syntax error —
/// must NOT be swept into the generic shape.
#[test]
fn pg_verbatim_messages_are_left_alone() {
    let mut e = fixture();
    assert_eq!(
        err(&mut e, "SELECT * FROM t LIMIT -1"),
        "parse: LIMIT must not be negative",
    );
    assert_eq!(
        err(&mut e, "SELECT 'abc'::bigint"),
        "eval: type mismatch: invalid input syntax for type bigint: \"abc\"",
    );
}
