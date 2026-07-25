//! read01 round 435 (MySQL differential) — DDL implicitly COMMITs an open
//! transaction, and a bare COMMIT / ROLLBACK is a no-op.
//!
//! PG runs DDL inside the transaction; MySQL commits before it. So
//! `START TRANSACTION; INSERT …; CREATE TABLE …; ROLLBACK` keeps the INSERT
//! on MySQL and **lost it** on SPG — silently, since nothing errors. A
//! MySQL application that treats a DDL step as a checkpoint got a different
//! database on SPG with no signal at all.
//!
//! Measured on MariaDB 11, the row written before the DDL SURVIVES the
//! rollback for: CREATE TABLE, ALTER TABLE, DROP TABLE, TRUNCATE, CREATE
//! INDEX, and a nested START TRANSACTION. Measured NOT to fire for
//! `CREATE TEMPORARY TABLE`, a `SET`, or a SELECT.
//!
//! The rule is a positive list of statements, not "anything that is not
//! DML": a statement wrongly on the list commits a client's data early,
//! which is as bad as the divergence it fixes.
//!
//! The trailing ROLLBACK then has nothing to roll back, which is how the
//! second half of this round was found: SPG answered "no active
//! transaction" as an ERROR, diverging from BOTH oracles. Measured — PG18
//! answers `WARNING: there is no transaction in progress` and still reports
//! ROLLBACK; MariaDB 11 succeeds silently. It is now a no-op in both
//! dialects, with PG's warning under the PG dialect.
//!
//! Every expectation is copied from a live MariaDB 11 / PG 18 run.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e.execute("CREATE TABLE t(i INT)").unwrap();
    e.execute("CREATE TABLE o(x INT)").unwrap();
    e
}

fn rows_of_t(e: &mut Engine) -> String {
    match e.execute("SELECT i FROM t").unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{other:?}"),
    }
}

/// Write a row inside a transaction, run `mid`, then ROLLBACK.
fn survives_rollback(mid: &str) -> String {
    let mut e = mysql();
    e.execute("START TRANSACTION").unwrap();
    e.execute("INSERT INTO t(i) VALUES (1)").unwrap();
    e.execute(mid).unwrap_or_else(|err| panic!("{mid}: {err}"));
    e.execute("ROLLBACK").unwrap_or_else(|err| panic!("ROLLBACK: {err}"));
    rows_of_t(&mut e)
}

#[test]
fn round435_ddl_commits_the_pending_write() {
    for ddl in [
        "CREATE TABLE other(y INT)",
        "ALTER TABLE o ADD COLUMN z INT",
        "DROP TABLE o",
        "TRUNCATE TABLE o",
        "CREATE INDEX ix ON o(x)",
    ] {
        assert_eq!(survives_rollback(ddl), "1", "{ddl} should have committed");
    }
}

#[test]
fn round435_nested_start_transaction_commits_the_pending_write() {
    // MySQL commits and opens a fresh transaction; SPG used to reject the
    // second START TRANSACTION outright with "a transaction is already open".
    assert_eq!(survives_rollback("START TRANSACTION"), "1");
}

#[test]
fn round435_temporary_table_does_not_commit() {
    // MariaDB's documented exception, measured.
    assert_eq!(survives_rollback("CREATE TEMPORARY TABLE tmp(x INT)"), "");
}

#[test]
fn round435_non_ddl_statements_do_not_commit() {
    assert_eq!(survives_rollback("SET @x = 1"), "");
    assert_eq!(survives_rollback("SELECT 1"), "");
}

#[test]
fn round435_plain_dml_rollback_still_rolls_back() {
    let mut e = mysql();
    e.execute("START TRANSACTION").unwrap();
    e.execute("INSERT INTO t(i) VALUES (6)").unwrap();
    e.execute("ROLLBACK").unwrap();
    assert_eq!(rows_of_t(&mut e), "");
}

#[test]
fn round435_bare_commit_and_rollback_are_no_ops() {
    // Both oracles accept these outside a transaction; SPG used to error.
    for dialect_is_mysql in [true, false] {
        let mut e = Engine::new();
        if dialect_is_mysql {
            e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
        }
        e.execute("ROLLBACK")
            .unwrap_or_else(|err| panic!("bare ROLLBACK (mysql={dialect_is_mysql}): {err}"));
        e.execute("COMMIT")
            .unwrap_or_else(|err| panic!("bare COMMIT (mysql={dialect_is_mysql}): {err}"));
    }
}

#[test]
fn round435_pg_dialect_keeps_ddl_inside_the_transaction() {
    // PG's whole point: DDL rolls back with everything else.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(i INT)").unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO t(i) VALUES (1)").unwrap();
    e.execute("CREATE TABLE other(y INT)").unwrap();
    e.execute("ROLLBACK").unwrap();
    assert_eq!(rows_of_t(&mut e), "");
    // …and the table the rolled-back DDL created is gone too.
    e.execute("SELECT * FROM other")
        .expect_err("the rolled-back CREATE TABLE must not survive");
}
