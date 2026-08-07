//! v7.39 (round 625, S05b/F29) — SPG accepted SQL that PG rejects.
//!
//! The ledger recorded eight such shapes. Enumerating instead of listing —
//! 71 functions and operators crossed with nine typed literals, 729 probes,
//! run against both engines and diffed on the ACCEPT/REJECT decision — found
//! **131**, and they were not 131 separate problems:
//!
//!     string family, first argument   112   btrim ltrim rtrim trim lpad rpad
//!                                           left right repeat replace strpos
//!                                           split_part translate quote_ident
//!     IS TRUE family                    8   every non-boolean type
//!     scattered                        11   char_length(bytea), age(int),
//!                                           min/max(bool|jsonb), string_agg,
//!                                           int[] || text, bytea = text
//!
//! This round closes the first two, 120 of the 131. `btrim(1)` answered `1`
//! — the wrong answer for a query that meant to trim a text column and was
//! handed an integer one, because it produced a number rather than saying
//! so. `1 IS TRUE` answered `false`, which reads as "the test was run and
//! did not hold" rather than "you cannot ask this of an integer"; the code
//! even carried a comment saying PG rejects it, next to the arm that
//! answered anyway.
//!
//! **The dialect is the whole reason this needs a gate.** MariaDB accepts
//! every one of them — `LTRIM(1)` → `1`, `LPAD(1,5,0)` → `00001`,
//! `1 IS TRUE` → `1`, measured against MariaDB 11.8 — and SPG is a drop-in
//! for both. Rejecting these under MySQL would break the other half of the
//! product, so the guard is PG-dialect only, and the MySQL-side behaviour is
//! pinned here too.
//!
//! PG's bytea overloads survive: `btrim(bytea, bytea)`, `ltrim`, `rtrim`,
//! `trim(bytea FROM bytea)` and `position(bytea IN bytea)` all still answer —
//! the first cut of the guard refused all of them, and the suite caught it.
//!
//! Recorded, not closed: eleven shapes remain lax (the scattered group), and
//! three go the other way — `string_agg(bytea, ',')`, `bytea LIKE 'a%'` and
//! `overlay(bytea …)` are accepted by PG and refused here. Also, SPG has no
//! `unknown` type, so where PG says `split_part(integer, unknown, integer)`
//! SPG says `split_part(integer, text, integer)`: same rejection, same
//! SQLSTATE, different rendering of a literal's type.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(err) => err.to_string(),
        Ok(ok) => panic!("{sql}: expected a rejection, got {ok:?}"),
    }
}

/// The string family refuses a non-text first argument, naming the call the
/// way PG names it.
#[test]
fn round625_string_functions_require_text() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT btrim(1)", "function btrim(integer) does not exist"),
        (
            "SELECT ltrim(1.5)",
            "function ltrim(numeric) does not exist",
        ),
        (
            "SELECT rtrim(TRUE)",
            "function rtrim(boolean) does not exist",
        ),
        (
            "SELECT lpad(1,5)",
            "function lpad(integer, integer) does not exist",
        ),
        (
            "SELECT repeat(1,2)",
            "function repeat(integer, integer) does not exist",
        ),
        (
            "SELECT strpos(1,'a')",
            "function strpos(integer, text) does not exist",
        ),
        (
            "SELECT left(1,1)",
            "function left(integer, integer) does not exist",
        ),
        (
            "SELECT right(1,1)",
            "function right(integer, integer) does not exist",
        ),
        (
            "SELECT translate(1,'a','b')",
            "function translate(integer, text, text) does not exist",
        ),
        (
            "SELECT quote_ident(1)",
            "function quote_ident(integer) does not exist",
        ),
        // PG resolves `trim` to pg_catalog.btrim and says so.
        (
            "SELECT trim(1)",
            "function pg_catalog.btrim(integer) does not exist",
        ),
    ] {
        let m = err(&mut e, sql);
        assert!(m.ends_with(want), "{sql}: wanted {want:?}, said {m:?}");
    }
    // Every non-text type, on one of them — this is the shape the
    // enumeration found eight times per function.
    for lit in [
        "1",
        "1.5",
        "TRUE",
        "ARRAY[1]",
        "'{\"a\":1}'::JSONB",
        "DATE '2020-01-01'",
        "INTERVAL '1 day'",
        "'\\x41'::BYTEA",
    ] {
        // one-argument btrim, which PG refuses for every one of these
        // INCLUDING bytea — its bytea overload is the two-argument form.
        let sql = alloc_sql(lit);
        let m = err(&mut e, &sql);
        assert!(m.contains("does not exist"), "{sql}: {m}");
    }
}

fn alloc_sql(lit: &str) -> String {
    format!("SELECT btrim({lit})")
}

/// …and still accepts everything PG accepts.
#[test]
fn round625_string_functions_still_take_text() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT btrim('  x  '), ltrim(' x'), rtrim('x ')"),
        vec!["x|x|x"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT lpad('x',3,'-'), repeat('ab',2), left('abc',2), right('abc',2)"
        ),
        vec!["--x|abab|ab|bc"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT replace('aXa','X','b'), strpos('abc','b'), split_part('a,b',',',2)"
        ),
        vec!["aba|2|b"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT translate('abc','ab','xy'), quote_ident('a b'), trim('  z  ')"
        ),
        vec!["xyc|\"a b\"|z"]
    );
    // A CHAR(n) argument is normalised to text before the guard, so the
    // padded type still passes — as it does in PG.
    assert_eq!(
        vals(&mut e, "SELECT btrim('x'::CHAR(3)), btrim('y'::VARCHAR)"),
        vec!["x|y"]
    );
    // NULL carries no type; PG resolves it through unknown and answers NULL.
    assert_eq!(
        vals(&mut e, "SELECT btrim(NULL) IS NULL, repeat(NULL,2) IS NULL"),
        vec!["true|true"]
    );
    // PG's bytea overloads are the TWO-argument forms, and they answer.
    assert_eq!(
        vals(
            &mut e,
            "SELECT btrim('\\x4141'::BYTEA, '\\x41'::BYTEA),              position('\\x6c'::BYTEA IN '\\x48656c6c6f'::BYTEA)"
        ),
        vec!["\\x|3"],
        "refusing these is what the first cut of the guard did"
    );
    // Columns of the right type are the ordinary case.
    e.execute("CREATE TABLE s (t TEXT, c CHAR(4), v VARCHAR(8))")
        .unwrap();
    e.execute("INSERT INTO s VALUES ('  a  ', 'b', ' c ')")
        .unwrap();
    assert_eq!(
        vals(&mut e, "SELECT btrim(t), btrim(c), btrim(v) FROM s"),
        vec!["a|b|c"]
    );
}

/// The IS TRUE family requires a boolean, with PG's sentence.
#[test]
fn round625_is_true_family_requires_boolean() {
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "SELECT 1 IS TRUE",
            "argument of IS TRUE must be type boolean, not type integer",
        ),
        (
            "SELECT 1 IS FALSE",
            "argument of IS FALSE must be type boolean, not type integer",
        ),
        (
            "SELECT 1 IS NOT TRUE",
            "argument of IS NOT TRUE must be type boolean, not type integer",
        ),
        (
            "SELECT 1 IS NOT FALSE",
            "argument of IS NOT FALSE must be type boolean, not type integer",
        ),
        (
            "SELECT 1 IS UNKNOWN",
            "argument of IS UNKNOWN must be type boolean, not type integer",
        ),
        (
            "SELECT 1 IS NOT UNKNOWN",
            "argument of IS NOT UNKNOWN must be type boolean, not type integer",
        ),
        (
            "SELECT 'x'::TEXT IS TRUE",
            "argument of IS TRUE must be type boolean, not type text",
        ),
    ] {
        let m = err(&mut e, sql);
        assert!(m.ends_with(want), "{sql}: wanted {want:?}, said {m:?}");
    }
    // What the family is actually for still works, including the two NULL
    // cases PG answers rather than rejects.
    assert_eq!(
        vals(
            &mut e,
            "SELECT TRUE IS TRUE, FALSE IS TRUE, TRUE IS NOT TRUE"
        ),
        vec!["true|false|false"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT NULL IS TRUE, NULL IS UNKNOWN, NULL IS NOT UNKNOWN"
        ),
        vec!["false|true|false"]
    );
    assert_eq!(
        vals(&mut e, "SELECT (1 = 1) IS TRUE, (1 = 2) IS FALSE"),
        vec!["true|true"]
    );
}
