//! read01 round 323 (V24) — a parse error reads like PG's.
//!
//! `ParseError::Display` prefixed EVERY parse error with
//! `parse error at token #N: `, an SPG token index PG has no equivalent
//! of. The message bodies underneath are already PG's verbatim — measured
//! on PG 18.4, `SELECT 1 LIMIT -1` is exactly `LIMIT must not be
//! negative` — so the prefix was the whole difference.
//!
//! The wire layers strip SPG's remaining internal class vocabulary
//! (`parse: ` / `eval: ` / `unsupported: `); this file pins the engine
//! side, and `e2e_parse_error_wire_round323` (spg-server) pins that
//! neither wire leaks it.

use spg_engine::Engine;

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}"),
    }
}

/// No token index, on any parse error.
#[test]
fn no_parse_error_carries_a_token_index() {
    let mut e = Engine::new();
    for sql in [
        "SELECT * FROM",
        "SELECT 1 LIMIT -1",
        "CREATE TABLE t (a int) WITH",
        "INSERT INTO",
        "SELECT (",
    ] {
        let msg = err(&mut e, sql);
        assert!(
            !msg.contains("parse error at token"),
            "`{sql}` still carries SPG's token index: {msg}"
        );
        assert!(
            !msg.contains('#'),
            "`{sql}` still carries a token marker: {msg}"
        );
    }
}

/// The body is PG's, verbatim, with nothing wrapped around it but the
/// engine's own layer prefix (which the wire strips).
#[test]
fn a_parse_error_body_is_pgs_own_text() {
    let mut e = Engine::new();
    assert_eq!(
        err(&mut e, "SELECT 1 LIMIT -1"),
        "parse: LIMIT must not be negative",
        "PG 18.4 says exactly `LIMIT must not be negative`"
    );
}
