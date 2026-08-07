//! read01 round 419 (MySQL differential) — the MySQL upsert lowerings apply
//! to EVERY INSERT source form, not just VALUES.
//!
//! `ON DUPLICATE KEY UPDATE` and `REPLACE INTO` were wired into the VALUES
//! branch of the INSERT parser only, so the bulk-upsert spellings every
//! MySQL ETL job uses fell over:
//!   INSERT INTO t SELECT … ON DUPLICATE KEY UPDATE c = VALUES(c)
//!       -> parse error at "ON"
//!   REPLACE INTO t SELECT …
//!       -> "duplicate key value violates unique constraint" (the REPLACE
//!          never lowered, so the row collided instead of replacing)
//! The VALUES spellings of both worked all along — this was an
//! inconsistency between source forms, not a missing feature.
//!
//! All four source forms (VALUES / SELECT / parenthesized source / WITH)
//! now resolve their conflict clause through one helper. A PostgreSQL
//! session's own `ON CONFLICT` on a SELECT source is unchanged.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn int_of(e: &mut Engine, sql: &str) -> i64 {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Int(n) => i64::from(*n),
            Value::BigInt(n) => *n,
            o => panic!("{o:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// `INSERT … SELECT … ON DUPLICATE KEY UPDATE c = VALUES(c)` takes the
/// incoming row's value.
#[test]
fn on_duplicate_key_over_select_takes_excluded() {
    let mut e = mysql();
    e.execute("CREATE TABLE t1(a INT PRIMARY KEY, b INT)")
        .unwrap();
    e.execute("INSERT INTO t1 VALUES(1,10)").unwrap();
    e.execute("INSERT INTO t1 SELECT 1, 99 ON DUPLICATE KEY UPDATE b = VALUES(b)")
        .unwrap();
    assert_eq!(int_of(&mut e, "SELECT b FROM t1"), 99);
}

/// The assignment may reference the EXISTING row's column too
/// (`b = b + 100` reads the stored 50).
#[test]
fn on_duplicate_key_over_select_reads_existing() {
    let mut e = mysql();
    e.execute("CREATE TABLE t2(a INT PRIMARY KEY, b INT)")
        .unwrap();
    e.execute("INSERT INTO t2 VALUES(5,50)").unwrap();
    e.execute("INSERT INTO t2 SELECT 5, 1 ON DUPLICATE KEY UPDATE b = b + 100")
        .unwrap();
    assert_eq!(int_of(&mut e, "SELECT b FROM t2"), 150);
}

/// `REPLACE INTO … SELECT` replaces the colliding row instead of failing.
#[test]
fn replace_into_select_replaces() {
    let mut e = mysql();
    e.execute("CREATE TABLE r1(a INT PRIMARY KEY, b INT)")
        .unwrap();
    e.execute("INSERT INTO r1 VALUES(1,1)").unwrap();
    e.execute("REPLACE INTO r1 SELECT 1, 2").unwrap();
    assert_eq!(int_of(&mut e, "SELECT b FROM r1"), 2);
    assert_eq!(int_of(&mut e, "SELECT COUNT(*) FROM r1"), 1);
}

/// `INSERT IGNORE … SELECT` skips the colliding row (keeps the old value).
#[test]
fn insert_ignore_select_skips() {
    let mut e = mysql();
    e.execute("CREATE TABLE s1(a INT PRIMARY KEY, b INT)")
        .unwrap();
    e.execute("INSERT INTO s1 VALUES(1,1)").unwrap();
    e.execute("INSERT IGNORE INTO s1 SELECT 1, 99").unwrap();
    assert_eq!(int_of(&mut e, "SELECT b FROM s1"), 1);
}

/// The WITH-source form gets the same treatment.
#[test]
fn on_duplicate_key_over_with_source() {
    let mut e = mysql();
    e.execute("CREATE TABLE w1(a INT PRIMARY KEY, b INT)")
        .unwrap();
    e.execute("INSERT INTO w1 VALUES(1,1)").unwrap();
    e.execute(
        "INSERT INTO w1 WITH c AS (SELECT 1 x, 7 y) SELECT x, y FROM c \
         ON DUPLICATE KEY UPDATE b = VALUES(b)",
    )
    .unwrap();
    assert_eq!(int_of(&mut e, "SELECT b FROM w1"), 7);
}

/// The VALUES spellings still behave (no regression from the refactor).
#[test]
fn values_forms_unchanged() {
    let mut e = mysql();
    e.execute("CREATE TABLE v1(a INT PRIMARY KEY, b INT)")
        .unwrap();
    e.execute("INSERT INTO v1 VALUES(1,1)").unwrap();
    e.execute("INSERT INTO v1 VALUES(1,42) ON DUPLICATE KEY UPDATE b = VALUES(b)")
        .unwrap();
    assert_eq!(int_of(&mut e, "SELECT b FROM v1"), 42);
    e.execute("REPLACE INTO v1 VALUES(1,7)").unwrap();
    assert_eq!(int_of(&mut e, "SELECT b FROM v1"), 7);
    e.execute("INSERT IGNORE INTO v1 VALUES(1,999)").unwrap();
    assert_eq!(int_of(&mut e, "SELECT b FROM v1"), 7);
}

/// A PostgreSQL session's own ON CONFLICT over a SELECT source is unchanged
/// (both DO UPDATE and DO NOTHING).
#[test]
fn postgres_on_conflict_over_select_unchanged() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p1(a INT PRIMARY KEY, b INT)")
        .unwrap();
    e.execute("INSERT INTO p1 VALUES(1,1)").unwrap();
    e.execute("INSERT INTO p1 SELECT 1, 5 ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b")
        .unwrap();
    assert_eq!(int_of(&mut e, "SELECT b FROM p1"), 5);
    e.execute("INSERT INTO p1 SELECT 1, 9 ON CONFLICT DO NOTHING")
        .unwrap();
    assert_eq!(int_of(&mut e, "SELECT b FROM p1"), 5);
}
