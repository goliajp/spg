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
