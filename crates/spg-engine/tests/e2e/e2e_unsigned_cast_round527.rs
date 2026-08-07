//! v7.39 (round 527) — `CAST(… AS UNSIGNED)` at full BIGINT UNSIGNED width.
//!
//! The last unmeasured silent-wrong on the v7.37 audit's list. C14
//! (unsigned arithmetic overflow answering -1) no longer reproduces —
//! SPG raises MariaDB's 1690 — but measuring it found C13 alive and in a
//! worse form than the audit recorded. It says "type mismatch"; what
//! actually happens is:
//!
//!     CAST(18446744073709551615 AS UNSIGNED)   MariaDB 18446744073709551615
//!                                              SPG     9223372036854775807
//!
//! A different number, with nothing to say so. The cast ran every input
//! through `f64`, which loses precision above 2^53, and Rust's float→int
//! cast SATURATES rather than wrapping — so the answer was i64::MAX.
//!
//! Only the CAST was affected: measured against MariaDB 11, a BIGINT
//! UNSIGNED column stores, orders, compares and SUMs the full range
//! correctly, and so does a bare literal.
//!
//! Every expectation below is a MariaDB 11 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.set_backslash_escapes(true);
    e
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The full range survives the cast.
#[test]
fn round527_unsigned_cast_keeps_the_whole_range() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT CAST(18446744073709551615 AS UNSIGNED)"),
        "18446744073709551615"
    );
    // One past i64::MAX — the first value the old path could not carry.
    assert_eq!(
        text(&mut e, "SELECT CAST(9223372036854775808 AS UNSIGNED)"),
        "9223372036854775808"
    );
    // i64::MAX itself, which the old path happened to answer for
    // everything above it.
    assert_eq!(
        text(&mut e, "SELECT CAST(9223372036854775807 AS UNSIGNED)"),
        "9223372036854775807"
    );
    // And it still travels through arithmetic.
    assert_eq!(
        text(&mut e, "SELECT CAST(18446744073709551615 AS UNSIGNED) + 0"),
        "18446744073709551615"
    );
}

/// The behaviours that were already right, kept: a negative wraps
/// through the full u64 range, rounding is half-away-from-zero, and a
/// string contributes its leading number.
#[test]
fn round527_the_other_cast_rules_are_unchanged() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT CAST(-1 AS UNSIGNED)"),
        "18446744073709551615"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT CAST(2.5 AS UNSIGNED), CAST(-2.5 AS SIGNED), CAST('12abc' AS UNSIGNED)"
        ),
        "3|-3|12"
    );
    assert_eq!(
        text(&mut e, "SELECT CAST(42 AS SIGNED), CAST(42 AS UNSIGNED)"),
        "42|42"
    );
}

/// MariaDB names the offending expression in its out-of-range message
/// and quotes the user's own syntax. SPG answered PG's `::` spelling,
/// in a message going to a MySQL client, for a cast the client had just
/// written the other way.
#[test]
fn round527_out_of_range_message_quotes_mysql_syntax() {
    let mut e = engine();
    let err = e
        .execute("SELECT CAST(1 AS UNSIGNED) - 2")
        .expect_err("out of range");
    let msg = format!("{err}");
    assert!(
        msg.contains("BIGINT UNSIGNED value is out of range in 'cast(1 as unsigned) - 2'"),
        "message was {msg}"
    );
    // Minimal parentheses, as MariaDB writes them.
    let err2 = e
        .execute("SELECT CAST(1 AS UNSIGNED) * 0 - 5")
        .expect_err("out of range");
    assert!(
        format!("{err2}").contains("out of range in 'cast(1 as unsigned) * 0 - 5'"),
        "message was {err2}"
    );
}
