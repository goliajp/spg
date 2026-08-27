//! read01 round 375, re-measured in v7.38.17 — trailing spaces on the
//! MySQL default collation.
//!
//! This file used to open "the MySQL default collation is PAD SPACE",
//! and it ended "Every expectation is copied from a MariaDB 11 run".
//! Both sentences were true separately and wrong together: MariaDB's
//! default (`utf8mb4_uca1400_ai_ci`) is PAD SPACE, MySQL 8.0's
//! (`utf8mb4_0900_ai_ci`) is NO PAD, and SPG advertises `8.0.0-spg-v…`
//! on the MySQL wire. The pins had been calibrated against the engine
//! we do not claim to be.
//!
//! Re-measured on MySQL 9.7.2 in `utf8mb4_0900_ai_ci`, every one of them
//! inverts:
//!
//!     'a' = 'a '            0   (was pinned 1)
//!     '' = ' '              0   (was pinned 1)
//!     'a' < 'a '            1   (was pinned 0)
//!     WHERE t = 'a'         1   (was pinned 3)
//!     COUNT(DISTINCT t)     4   (was pinned 2)
//!     GROUP BY t -> groups  4   (was pinned 2)
//!     UNIQUE accepts 'a '   yes (was pinned: rejected)
//!
//! What did NOT move: a non-trailing space is still significant, a tab
//! is not a pad, `LIKE` still treats a trailing space literally, and
//! storage is untouched — `LENGTH('a ')` is 2.
//!
//! `CHAR(n)` is a different question with a different answer; see
//! `mysql_compare_fold_char`. Both engines ignore a CHAR's padding
//! because that is a property of the type.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn scalar(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
            .unwrap_or(Value::Null),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn count(e: &mut Engine, sql: &str) -> i64 {
    match scalar(e, sql) {
        Value::BigInt(n) => n,
        other => panic!("`{sql}` not a count: {other:?}"),
    }
}

/// Trailing spaces are DATA in a comparison — MySQL 9.7.2, NO PAD.
#[test]
fn comparison_ignores_trailing_spaces() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT 'a' = 'a '"), Value::Bool(false));
    assert_eq!(scalar(&mut e, "SELECT 'a' = 'a  '"), Value::Bool(false));
    assert_eq!(scalar(&mut e, "SELECT '' = ' '"), Value::Bool(false));
    // A padded value is GREATER, not equal: the shorter string sorts
    // first once its trailing spaces stop being ignored.
    assert_eq!(scalar(&mut e, "SELECT 'a' < 'a '"), Value::Bool(true));
    // A non-trailing space is significant.
    assert_eq!(scalar(&mut e, "SELECT 'a' < 'a b'"), Value::Bool(true));
    // Only spaces pad — a tab is significant.
    assert_eq!(scalar(&mut e, "SELECT 'a' = 'a\t'"), Value::Bool(false));
}

/// WHERE / DISTINCT / GROUP BY collapse space-padded variants.
#[test]
fn where_distinct_group_collapse_padding() {
    let mut e = mysql();
    e.execute("CREATE TABLE s (t VARCHAR(10))").unwrap();
    e.execute("INSERT INTO s VALUES ('a'),('a '),('a  '),('b')")
        .unwrap();
    // 'a', 'a ', 'a  ' and 'b' are four values to MySQL 9.7.2.
    assert_eq!(count(&mut e, "SELECT COUNT(*) FROM s WHERE t = 'a'"), 1);
    assert_eq!(count(&mut e, "SELECT COUNT(DISTINCT t) FROM s"), 4);
    assert_eq!(
        count(
            &mut e,
            "SELECT COUNT(*) FROM (SELECT t FROM s GROUP BY t) g"
        ),
        4
    );
}

/// A UNIQUE constraint treats `'a'` and `'a '` as DIFFERENT keys.
/// MySQL 9.7.2 accepts both and the table holds two rows.
#[test]
fn unique_collapses_padding() {
    let mut e = mysql();
    e.execute("CREATE TABLE u (t VARCHAR(10) UNIQUE)").unwrap();
    e.execute("INSERT INTO u VALUES ('a')").unwrap();
    e.execute("INSERT INTO u VALUES ('a ')")
        .expect("'a ' is not 'a' under NO PAD");
    assert_eq!(count(&mut e, "SELECT COUNT(*) FROM u"), 2);
}

/// LIKE and BINARY keep the trailing space significant.
#[test]
fn like_and_binary_keep_the_space() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT 'a ' LIKE 'a'"), Value::Bool(false));
    assert_eq!(scalar(&mut e, "SELECT 'a' LIKE 'a '"), Value::Bool(false));
    assert_eq!(
        scalar(&mut e, "SELECT 'a' = BINARY 'a '"),
        Value::Bool(false)
    );
    // Storage keeps the space — only comparison ignores it.
    assert_eq!(scalar(&mut e, "SELECT LENGTH('a ')"), Value::Int(2));
}

/// A PostgreSQL session compares byte-wise — trailing spaces matter.
#[test]
fn postgres_session_keeps_trailing_spaces() {
    let mut p = Engine::new();
    assert_eq!(scalar(&mut p, "SELECT 'a' = 'a '"), Value::Bool(false));
}

/// v7.39 — the session's collation decides, and it reaches a bare
/// literal.
///
/// The tests above pin the DEFAULT (`utf8mb4_0900_ai_ci`, NO PAD), and
/// they passed both before and after the fix this pins, because the
/// default is NO PAD either way: `pads_space(None)` and
/// `pads_space("utf8mb4_0900_ai_ci")` both answer false. A suite that
/// cannot go red for the change it is meant to cover is not covering it.
///
/// What was wrong: every `pads_space` call site took its collation from
/// a COLUMN, so `'a' = 'a '` — two literals, no column — asked nothing
/// and fell to `None`. Measured against MySQL 9.7.2 with both ends on
/// `utf8mb4_general_ci` (PAD SPACE, and SPG's own rule agrees): MySQL
/// answered 1, SPG answered 0, while the same comparison against a
/// column of that collation answered 1 on both.
#[test]
fn a_pad_space_session_collation_reaches_bare_literals() {
    let mut e = mysql();

    // Default: NO PAD, matching a bare `SET NAMES utf8mb4` on MySQL 9.7.2.
    assert_eq!(scalar(&mut e, "SELECT 'a' = 'a '"), Value::Bool(false));
    assert_eq!(scalar(&mut e, "SELECT 'a ' IN ('a')"), Value::Bool(false));

    // Ask for a PAD SPACE collation and both must flip.
    e.execute("SET collation_connection = 'utf8mb4_general_ci'")
        .unwrap();
    assert_eq!(
        scalar(&mut e, "SELECT 'a' = 'a '"),
        Value::Bool(true),
        "a PAD SPACE session collation must reach a bare literal"
    );
    assert_eq!(
        scalar(&mut e, "SELECT 'a ' IN ('a')"),
        Value::Bool(true),
        "…and the membership test too"
    );

    // And back: an explicitly NO PAD name flips them again, which is what
    // says the session value is being READ rather than a constant.
    e.execute("SET collation_connection = 'utf8mb4_0900_ai_ci'")
        .unwrap();
    assert_eq!(scalar(&mut e, "SELECT 'a' = 'a '"), Value::Bool(false));
}

/// The PostgreSQL dialect must not pad, whatever the MySQL session says.
///
/// `pads_space` reads a MySQL collation NAME, and a PostgreSQL database
/// collating as `en_US.utf8` does not pad — feeding an inherited name to
/// it there would make `'a' = 'a  '` true for every text column in such
/// a database. That sentence is from v7.38.18 and it names the input
/// this test needs: a **column**, not a literal.
///
/// Two earlier versions of this test used literals and could not fail.
/// Ablated three ways — gate removed, and then
/// `session_collation_name` rewritten to return a PAD SPACE name
/// unconditionally — a literal comparison on the PG dialect never
/// changed its answer, because it does not reach `text_compare`'s
/// `pads` at all. `text_compare` is where a COLUMN comparison lands,
/// and a PG column with no collation of its own is exactly what would
/// inherit the session name if the gate were gone.
#[test]
fn a_pad_space_the_pg_dialect_never_pads() {
    let mut e = Engine::new(); // no MySQL sql_mode: PG dialect
    assert_eq!(scalar(&mut e, "SELECT 'a' = 'a  '"), Value::Bool(false));

    // The discriminating input: a plain PG text column, compared against
    // a padded literal, with a PAD SPACE collation named on the session.
    e.execute("CREATE TABLE pg_pad (s TEXT)").unwrap();
    e.execute("INSERT INTO pg_pad VALUES ('a')").unwrap();
    e.execute("SET collation_connection = 'utf8mb4_general_ci'")
        .unwrap();
    assert_eq!(
        count(&mut e, "SELECT count(*) FROM pg_pad WHERE s = 'a  '"),
        0,
        "a MySQL collation name must not reach a PostgreSQL column comparison"
    );
    assert_eq!(
        count(&mut e, "SELECT count(*) FROM pg_pad WHERE s IN ('a  ')"),
        0,
        "…nor its membership test"
    );
}
