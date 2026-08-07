//! read01 round 426 (MySQL differential) — `ROW_COUNT()`.
//!
//! MySQL's canonical "did that write hit anything" idiom — ORMs and
//! migration tools gate on it. SPG returned a hard-coded -1 forever, so
//! every such check read "no rows / not a DML statement" no matter what the
//! statement did. Measured on MariaDB 11:
//!
//!   after DDL                          0
//!   INSERT of 3 rows                   3
//!   UPDATE that CHANGED 2              2
//!   UPDATE that matched 3, changed 0   0   <- changed, NOT matched
//!   DELETE of 1                        1
//!   DELETE matching nothing            0
//!   after a row-returning statement   -1
//!
//! The statement driver now stamps the count per session (beside
//! `last_insert_id`, the round-347 shape), and the MySQL affected-count for
//! UPDATE counts rows whose values actually changed — PG's UPDATE tag
//! counts every matched row, so that part is dialect-gated.
//!
//! SCOPE — the upsert quirks are NOT modelled: MariaDB reports 2 for an
//! `ON DUPLICATE KEY UPDATE` that changed a row and 0 for one that did not,
//! and REPLACE has its own delete+insert accounting. Those sub-rules were
//! only partly pinned down against the oracle, and a half-understood rule
//! is worse than the plain count; `upsert_counts_are_not_modelled` records
//! today's answer so the follow-up round has a baseline.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn row_count(e: &mut Engine) -> i64 {
    match e.execute("SELECT ROW_COUNT()").unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::BigInt(n) => *n,
            Value::Int(n) => i64::from(*n),
            o => panic!("{o:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn affected(e: &mut Engine, sql: &str) -> usize {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::CommandOk { affected, .. } => affected,
        other => panic!("{sql}: {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = mysql();
    e.execute("CREATE TABLE t(id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO t VALUES(1,10),(2,20),(3,30)")
        .unwrap();
    e
}

/// DDL leaves 0; an INSERT leaves the row count it wrote.
#[test]
fn ddl_then_insert() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(id INT PRIMARY KEY, v INT)")
        .unwrap();
    assert_eq!(row_count(&mut e), 0);
    e.execute("INSERT INTO t VALUES(1,10),(2,20),(3,30)")
        .unwrap();
    assert_eq!(row_count(&mut e), 3);
}

/// UPDATE reports rows CHANGED, not rows matched.
#[test]
fn update_reports_changed_not_matched() {
    let mut e = seeded();
    // Matches all three, changes none.
    e.execute("UPDATE t SET v = v WHERE 1 = 1").unwrap();
    assert_eq!(row_count(&mut e), 0);
    // Changes two.
    e.execute("UPDATE t SET v = v + 1 WHERE id <= 2").unwrap();
    assert_eq!(row_count(&mut e), 2);
}

/// DELETE reports what it removed, including zero.
#[test]
fn delete_counts() {
    let mut e = seeded();
    e.execute("DELETE FROM t WHERE id = 1").unwrap();
    assert_eq!(row_count(&mut e), 1);
    e.execute("DELETE FROM t WHERE id = 99").unwrap();
    assert_eq!(row_count(&mut e), 0);
}

/// A row-returning statement leaves -1 — which is also why two ROW_COUNT()
/// calls in a row answer -1 the second time: the first one's SELECT is
/// itself "the last statement".
#[test]
fn row_returning_statement_leaves_minus_one() {
    let mut e = seeded();
    e.execute("DELETE FROM t WHERE id = 1").unwrap();
    assert_eq!(row_count(&mut e), 1);
    // The SELECT above was itself a row-returning statement.
    assert_eq!(row_count(&mut e), -1);
}

/// A fresh session reads 0 — measured; the never-ran state is NOT -1 (only
/// a row-returning statement leaves that). `SET sql_mode=…` likewise leaves
/// 0, so entering the MySQL dialect does not disturb the answer.
#[test]
fn fresh_session_reads_zero() {
    let mut e = Engine::new();
    assert_eq!(row_count(&mut e), 0);
    let mut m = mysql();
    assert_eq!(row_count(&mut m), 0);
}

/// The MySQL changed-count also drives the statement's own affected tag.
#[test]
fn mysql_update_tag_counts_changed() {
    let mut e = seeded();
    assert_eq!(affected(&mut e, "UPDATE t SET v = v WHERE 1 = 1"), 0);
    assert_eq!(affected(&mut e, "UPDATE t SET v = v + 1 WHERE id <= 2"), 2);
}

/// A PostgreSQL session's UPDATE tag still counts every MATCHED row — each
/// gets a new row version there, which is what PG reports.
#[test]
fn postgres_update_tag_counts_matched() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO t VALUES(1,10),(2,20),(3,30)")
        .unwrap();
    assert_eq!(affected(&mut e, "UPDATE t SET v = v WHERE 1 = 1"), 3);
}

/// Round 427 modelled the upsert accounting after measuring the sub-rules
/// exhaustively — see `e2e_mysql_upsert_count_round427` for the full table.
#[test]
fn upsert_counts_match_mariadb() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO t VALUES(1,10),(2,20)").unwrap();
    // A conflict that CHANGED the row counts as delete+insert.
    e.execute("INSERT INTO t VALUES(2,999) ON DUPLICATE KEY UPDATE v = 999")
        .unwrap();
    assert_eq!(row_count(&mut e), 2);
    // A pure insert counts 1.
    e.execute("INSERT INTO t VALUES(9,9) ON DUPLICATE KEY UPDATE v = 9")
        .unwrap();
    assert_eq!(row_count(&mut e), 1);
}
