//! Round 704 — four of the five expression-error gaps the r703 probe batch
//! measured (S05g ①). The fifth — an unreferenced WINDOW clause resolving
//! its columns — carries a 25-site AST change and is the next round, per
//! the plan.
//!
//! All four are the same disease in different clothes: SPG described a
//! failure in its own words where PG18 has a specific sentence, and in two
//! of the four SPG's words were also the wrong DIAGNOSIS.
//!
//!   * `lag(i)` with no OVER said `function lag(integer) does not exist` —
//!     for a function SPG has. A reader goes checking spelling and
//!     extensions. PG names the actual mistake: the missing OVER.
//!
//!   * `i = 'abc'` (int column) said `operator does not exist: integer =
//!     text` — but the operator exists; the LITERAL failed. The comment at
//!     the coercion site claimed the fall-through "matched PG" — an
//!     unverified PG assertion, the F31 comment shape. PG commits the
//!     unknown literal to the column's type and reports the input
//!     function's error. Only the numeric family propagates this way:
//!     for `json` and friends PG refuses the OPERATOR before parsing, so
//!     their fall-through really was the match.
//!
//!   * `substring(i FROM 1 FOR 2)` and `d > 'notadate'` were wording-only.

use spg_engine::{Engine, QueryResult};

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(&format!("PG18 refuses: {sql}")))
}

fn seed(e: &mut Engine) {
    e.execute("CREATE TABLE t704(i INT, b BIGINT, f FLOAT, d DATE, ts TIMESTAMP)")
        .unwrap();
    e.execute("INSERT INTO t704 VALUES (5, 5, 5, '2020-01-01', '2020-01-01 00:00:00')")
        .unwrap();
}

#[test]
fn round704_a_bare_window_function_names_the_missing_over() {
    let mut e = Engine::new();
    seed(&mut e);
    for func in ["lag(i)", "lead(i)", "row_number()", "rank()", "ntile(4)"] {
        let err = err_of(&mut e, &format!("SELECT {func} FROM t704"));
        assert!(
            err.contains("requires an OVER clause"),
            "{func}: {err}"
        );
        assert!(
            !err.contains("does not exist"),
            "{func} exists; the error must not deny it: {err}"
        );
    }
    // A name that truly does not exist still says so.
    assert!(err_of(&mut e, "SELECT nosuchfn704(i) FROM t704").contains("does not exist"));
}

/// PG's sentence, and PG's diagnosis: the value failed, not the operator.
#[test]
fn round704_a_bad_numeric_literal_fails_as_a_value() {
    let mut e = Engine::new();
    seed(&mut e);
    for (sql, want) in [
        (
            "SELECT i FROM t704 WHERE i = 'abc'",
            "invalid input syntax for type integer: \"abc\"",
        ),
        (
            "SELECT i FROM t704 WHERE b > 'x'",
            "invalid input syntax for type bigint: \"x\"",
        ),
        (
            "SELECT i FROM t704 WHERE f = 'y'",
            "invalid input syntax for type double precision: \"y\"",
        ),
    ] {
        let err = err_of(&mut e, sql);
        assert!(err.contains(want), "{sql}\n  got: {err}\n  want: {want}");
        assert!(!err.contains("operator does not exist"), "{sql}: {err}");
    }
    // The lift itself is untouched: a literal that parses still compares,
    // whitespace and all.
    let n = match e
        .execute("SELECT count(*) FROM t704 WHERE i = '5' AND i > '  4 '")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    };
    assert_eq!(n, "1");
}

#[test]
fn round704_substring_over_an_int_is_a_missing_overload() {
    let mut e = Engine::new();
    seed(&mut e);
    assert!(
        err_of(&mut e, "SELECT substring(i FROM 1 FOR 2) FROM t704")
            .contains("function pg_catalog.substring(integer, integer, integer) does not exist"),
    );
    // The working overloads did not move.
    let got = match e
        .execute("SELECT substring('hello' FROM 2 FOR 3)")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    };
    assert_eq!(got, "ell");
}

#[test]
fn round704_a_temporal_literal_that_wont_lift_uses_the_input_functions_words() {
    let mut e = Engine::new();
    seed(&mut e);
    assert!(
        err_of(&mut e, "SELECT i FROM t704 WHERE d > 'notadate'")
            .contains("invalid input syntax for type date: \"notadate\""),
    );
    assert!(
        err_of(&mut e, "SELECT i FROM t704 WHERE ts > 'nope'")
            .contains("invalid input syntax for type timestamp: \"nope\""),
    );
}
