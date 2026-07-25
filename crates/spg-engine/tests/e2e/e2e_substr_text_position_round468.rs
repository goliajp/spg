//! read01 round 468 (C9) — a TEXT position argument to substr / substring.
//!
//! `substr('abcdef','2')` answered NULL. PG18 answers `bcdef` and MariaDB 11
//! answers `bcdef`; both coerce the argument. A NULL where the server has a
//! perfectly good answer is silent and wrong, and it reached the caller as a
//! missing value rather than an error.
//!
//! Two separate causes, one call site:
//!
//!   * The positional path refused a TEXT argument outright, so anything
//!     that fell through to it answered NULL.
//!   * `substr` and `mid` were sharing PG's `substring(string, pattern)`
//!     regex reading. PG has no such overload for `substr` (it coerces the
//!     literal), and MySQL has no regex form at all — `SUBSTRING('abcdef',
//!     '2')` is `bcdef` there, not a pattern match.
//!
//! Every expectation is copied from a live PG18 / MariaDB 11 run.

use spg_engine::{Engine, QueryResult};

fn pg() -> Engine {
    Engine::new()
}

fn my() -> Engine {
    let mut e = Engine::new();
    e.set_backslash_escapes(true);
    e
}

fn one(e: &mut Engine, sql: &str) -> Result<String, String> {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => {
            Ok(spg_engine::eval::value_to_text(&rows[0].values[0]))
        }
        Ok(other) => Err(format!("{other:?}")),
        Err(err) => Err(format!("{err}")),
    }
}

/// (sql, PG18's answer). NULL renders as the empty string, so the two cases
/// that PG answers with an empty result are asserted separately below.
const PG_ANSWERS: &[(&str, &str)] = &[
    ("SELECT substr('abcdef','2')", "bcdef"),
    ("SELECT substr('abcdef', 2)", "bcdef"),
    ("SELECT substr('abcdef','2','3')", "bcd"),
    // PG coerces with whitespace trimmed.
    ("SELECT substr('abcdef',' 2 ')", "bcdef"),
    // PG clamps a negative start to 1 and counts the length from there.
    ("SELECT substr('abcdef','-1','3')", "a"),
    ("SELECT substr('abcdef','0')", "abcdef"),
    ("SELECT mid('abcdef','2','3')", "bcd"),
    // The regex reading survives where it belongs: `substring(str, pattern)`.
    ("SELECT substring('abcdef','bc')", "bc"),
    ("SELECT substring('abcdef', 2)", "bcdef"),
    ("SELECT substring('abcdef', 2, 3)", "bcd"),
];

/// (sql, MariaDB 11's answer).
const MY_ANSWERS: &[(&str, &str)] = &[
    ("SELECT substr('abcdef','2')", "bcdef"),
    ("SELECT substr('abcdef','2','3')", "bcd"),
    ("SELECT substr('abcdef',' 2 ')", "bcdef"),
    // MySQL counts a negative start from the END.
    ("SELECT substr('abcdef','-1','3')", "f"),
    // MySQL never raises here: it reads the leading integer and the rest is
    // zero, and position 0 is the empty string.
    ("SELECT substr('abcdef','x')", ""),
    ("SELECT substr('abcdef','')", ""),
    ("SELECT substr('abcdef','0')", ""),
    // A STRING '2.7' truncates to 2 (a numeric 2.7 would round to 3).
    ("SELECT substr('abcdef','2.7')", "bcdef"),
    // MySQL has no regex form — this is a position, and 'bc' reads as 0.
    ("SELECT substring('abcdef','2')", "bcdef"),
    ("SELECT substring('abcdef','bc')", ""),
    ("SELECT mid('abcdef','2','3')", "bcd"),
];

#[test]
fn round468_pg_coerces_a_text_position() {
    let mut e = pg();
    for (sql, want) in PG_ANSWERS {
        assert_eq!(one(&mut e, sql).as_deref(), Ok(*want), "for `{sql}`");
    }
}

#[test]
fn round468_pg_raises_on_a_position_that_is_not_an_integer() {
    // PG18: ERROR: invalid input syntax for type integer: "x"
    let mut e = pg();
    for (sql, bad) in [
        ("SELECT substr('abcdef','x')", "\"x\""),
        ("SELECT substr('abcdef','2.7')", "\"2.7\""),
        ("SELECT substr('abcdef','')", "\"\""),
    ] {
        let Err(msg) = one(&mut e, sql) else {
            panic!("`{sql}` answered; PG raises");
        };
        assert!(
            msg.contains("invalid input syntax for type integer")
                && msg.contains(bad),
            "`{sql}` gave: {msg}"
        );
    }
}

#[test]
fn round468_pg_keeps_the_regex_reading_of_substring() {
    // `substring(string, pattern)` is PG's regex extraction and must stay
    // that way — `'2'` matches nothing in 'abcdef', so the answer is NULL.
    let mut e = pg();
    match e.execute("SELECT substring('abcdef','2') IS NULL").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[0]), "true");
        }
        other => panic!("{other:?}"),
    }
    // Out of range is an empty result in PG, not an error.
    assert_eq!(one(&mut e, "SELECT substr('abcdef','99')").as_deref(), Ok(""));
}

#[test]
fn round468_mysql_reads_the_leading_integer_and_never_raises() {
    let mut e = my();
    for (sql, want) in MY_ANSWERS {
        assert_eq!(one(&mut e, sql).as_deref(), Ok(*want), "for `{sql}`");
    }
}
