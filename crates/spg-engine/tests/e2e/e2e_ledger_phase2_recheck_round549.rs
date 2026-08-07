//! v7.39 (round 549) — re-measuring the audit's remaining silent-wrongs.
//!
//! Round 528 found that nine of the audit's fifteen capability entries
//! no longer reproduced as written, so working from the ledger without
//! re-measuring means scheduling the wrong work. Phase 2 — the
//! silent-wrong list, which outranks everything else in that ledger —
//! still carried three entries. All three were measured against live
//! PG18 and MariaDB 11 this round:
//!
//!     S-3  temporary sequence / view created as PERMANENT
//!          → no longer reproduces (round 526 fixed relpersistence)
//!     S-2  substr's 2-arg form coerced a text position to NULL
//!          → no longer reproduces
//!     S-1  MySQL UNSIGNED arithmetic overflowed silently
//!          → NARROWS to one case; the rest already raise 1690
//!
//! What survives of S-1 is a literal ABOVE i64::MAX. MariaDB reads it
//! as BIGINT UNSIGNED and raises 1690 on overflow; SPG reads it as
//! numeric — PG's rule — and answers 18446744073709551616. That is the
//! BIGINT UNSIGNED representation decision the ledger already carries
//! as C13/A-3, not something to settle at the tail of a round.
//!
//! These pins exist so the ledger cannot be re-trusted blindly: what
//! matches now is held in place, and what does not is stated exactly.
//!
//! Every expectation below is a live PG18 or MariaDB 11 reading.

use spg_engine::{Engine, QueryResult};

fn mysql_session() -> Engine {
    let mut e = Engine::new();
    // What a mysql client's preamble does; it selects the dialect.
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
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

/// S-3: a TEMPORARY sequence and view say so, as PG's do.
#[test]
fn round549_s3_temp_objects_are_temporary() {
    let mut e = Engine::new();
    e.execute("CREATE TEMPORARY SEQUENCE tseq").unwrap();
    e.execute("CREATE TEMPORARY VIEW tv AS SELECT 1 AS x")
        .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT relname, relkind, relpersistence FROM pg_class \
             WHERE relname IN ('tseq', 'tv') ORDER BY relname"
        ),
        vec!["tseq|S|t", "tv|v|t"]
    );
    // A permanent one still reads 'p'.
    e.execute("CREATE SEQUENCE pseq").unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT relpersistence FROM pg_class WHERE relname = 'pseq'"
        ),
        vec!["p"]
    );
}

/// S-2: substr's two-argument form takes a text position.
#[test]
fn round549_s2_substr_two_arg_coercion() {
    let mut e = Engine::new();
    assert_eq!(rows(&mut e, "SELECT substr('abcdef', '2')"), vec!["bcdef"]);
    assert_eq!(rows(&mut e, "SELECT substr('abcdef', 2)"), vec!["bcdef"]);
}

/// S-1, the overflow half — and the case this pin FOUND.
///
/// `9223372036854775807 + 1` and `… * 2` already raised. The negative
/// boundary did not: `-9223372036854775808 - 1` answered
/// -9223372036854775809, a value no bigint can hold, where PG18 raises.
///
/// The arithmetic was never the problem. `9223372036854775808` is one
/// past i64::MAX, so the lexer hands it over as a NUMERIC and unary
/// minus on a numeric stays numeric — SPG typed the literal `numeric`
/// where PG types it `bigint`. The sign is folded into the literal now,
/// as PG folds it.
#[test]
fn round549_s1_integer_overflow_raises() {
    for mut e in [Engine::new(), mysql_session()] {
        for sql in [
            "SELECT 9223372036854775807 + 1",
            // The one that used to answer instead of raising.
            "SELECT -9223372036854775808 - 1",
            "SELECT 9223372036854775807 * 2",
        ] {
            let err = format!("{}", e.execute(sql).expect_err(sql));
            assert!(
                err.to_ascii_lowercase().contains("out of range"),
                "{sql}: message was {err}"
            );
        }
        // And a sum that fits is untouched.
        assert_eq!(
            rows(&mut e, "SELECT 9223372036854775807 + 0"),
            vec!["9223372036854775807"]
        );
        // The literal types as PG types it — which is the whole fix.
        assert_eq!(
            rows(&mut e, "SELECT pg_typeof(-9223372036854775808)"),
            vec!["bigint"]
        );
        assert_eq!(
            rows(&mut e, "SELECT -9223372036854775808"),
            vec!["-9223372036854775808"]
        );
        // An ordinary negative literal is unchanged.
        assert_eq!(rows(&mut e, "SELECT -5 - 1"), vec!["-6"]);
        assert_eq!(rows(&mut e, "SELECT -1.5 - 1"), vec!["-2.5"]);
    }
}

/// S-1, the part that does NOT: a literal above i64::MAX.
///
/// PG18 reads it as numeric and answers 18446744073709551616; SPG
/// agrees with PG. MariaDB reads it as BIGINT UNSIGNED and raises 1690.
/// SPG follows PG in BOTH dialects, which is the open half of the
/// ledger's C13 — recorded here rather than left to be rediscovered.
#[test]
fn round549_s1_the_open_half_is_pgs_rule_in_both_dialects() {
    for mut e in [Engine::new(), mysql_session()] {
        assert_eq!(
            rows(&mut e, "SELECT 18446744073709551615 + 1"),
            vec!["18446744073709551616"],
            "SPG follows PG's numeric promotion here; MariaDB raises 1690"
        );
        // The literal itself round-trips exactly either way.
        assert_eq!(
            rows(&mut e, "SELECT 18446744073709551615"),
            vec!["18446744073709551615"]
        );
    }
    // PG's own typing of that literal, which is why the promotion happens.
    let mut e = Engine::new();
    assert_eq!(
        rows(&mut e, "SELECT pg_typeof(18446744073709551615)"),
        vec!["numeric"]
    );
}
