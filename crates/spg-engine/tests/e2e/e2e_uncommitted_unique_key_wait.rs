//! 8.0.3 — a unique key another transaction holds uncommitted.
//!
//! PostgreSQL gives the key to its FIRST WRITER. A later writer's
//! uniqueness check sees the in-progress row and waits for its
//! transaction to end; then it fails with 23505 if that transaction
//! committed, or proceeds if it rolled back. `ON CONFLICT DO NOTHING` /
//! `DO UPDATE` wait the same way and then see the committed row.
//!
//! SPG wrote each transaction into its own shadow, where no one else
//! could see the key, and re-checked uniqueness at COMMIT. So the later
//! writer won, and the FIRST writer — already told `INSERT 0 1` — was
//! refused at COMMIT with 40001 and lost its whole transaction. Reported
//! by sentori as a 500 on ingest whenever a new fault fired on many
//! devices at once, measured against PG 18.6 with two sessions:
//!
//! ```text
//!   B's statement          PG 18.6                    SPG 8.0.2
//!   plain INSERT           B 23505, A commits         B inserts, A 40001
//!   ON CONFLICT DO NOTHING B 0 rows, A commits        B inserts, A 40001
//!   ON CONFLICT DO UPDATE  B updates A's row          B inserts, A 40001
//! ```
//!
//! The engine never parks: a waiting statement returns `LockWouldBlock`
//! and the server retries it with the engine lock released, which is
//! what these pins emulate by re-running the statement. Every expected
//! outcome is PostgreSQL 18.6's for the same two sessions.

use spg_engine::{Engine, EngineError, IMPLICIT_TX, QueryResult, TxId};

fn engine(ddl: &str) -> Engine {
    let mut e = Engine::new();
    e.execute(ddl).unwrap_or_else(|x| panic!("{ddl}: {x:?}"));
    e
}

fn cell(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join(":")
            })
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

fn holder(e: &mut Engine, sql: &str) -> TxId {
    let a = e.alloc_tx_id();
    e.execute_in("BEGIN", a).unwrap();
    e.execute_in(sql, a)
        .unwrap_or_else(|x| panic!("{sql}: {x:?}"));
    a
}

fn waits(r: Result<QueryResult, EngineError>, what: &str) {
    match r {
        Err(EngineError::LockWouldBlock) => {}
        other => panic!("{what}: must wait for the holder, got {other:?}"),
    }
}

#[test]
fn a_plain_insert_of_a_held_key_waits_then_fails_and_the_first_writer_commits() {
    let mut e = engine("CREATE TABLE t (k int UNIQUE, n int)");
    let a = holder(&mut e, "INSERT INTO t VALUES (1, 1)");
    waits(
        e.execute_in("INSERT INTO t VALUES (1, 2)", IMPLICIT_TX),
        "B",
    );
    e.execute_in("COMMIT", a)
        .expect("the FIRST writer's COMMIT must succeed — it was told INSERT 0 1");
    let err = e
        .execute_in("INSERT INTO t VALUES (1, 2)", IMPLICIT_TX)
        .expect_err("after A commits, B's key is taken");
    assert!(
        format!("{err:?}").contains("duplicate key"),
        "B's retry must be a unique violation, got {err:?}"
    );
    assert_eq!(cell(&mut e, "SELECT k, n FROM t"), "1:1");
}

#[test]
fn do_nothing_waits_then_skips_the_row_the_holder_committed() {
    let mut e = engine("CREATE TABLE t (k int UNIQUE, n int)");
    let a = holder(&mut e, "INSERT INTO t VALUES (1, 1)");
    let b = "INSERT INTO t VALUES (1, 2) ON CONFLICT (k) DO NOTHING";
    waits(e.execute_in(b, IMPLICIT_TX), "DO NOTHING");
    e.execute_in("COMMIT", a).unwrap();
    e.execute_in(b, IMPLICIT_TX).unwrap();
    assert_eq!(cell(&mut e, "SELECT k, n FROM t"), "1:1");
}

#[test]
fn do_update_waits_then_updates_the_holders_row() {
    let mut e = engine("CREATE TABLE t (k int UNIQUE, n int)");
    let a = holder(&mut e, "INSERT INTO t VALUES (1, 1)");
    let b = "INSERT INTO t VALUES (1, 2) ON CONFLICT (k) DO UPDATE SET n = EXCLUDED.n";
    waits(e.execute_in(b, IMPLICIT_TX), "DO UPDATE");
    e.execute_in("COMMIT", a).unwrap();
    e.execute_in(b, IMPLICIT_TX).unwrap();
    assert_eq!(cell(&mut e, "SELECT k, n FROM t"), "1:2");
}

#[test]
fn a_rollback_frees_the_key() {
    let mut e = engine("CREATE TABLE t (k int UNIQUE, n int)");
    let a = holder(&mut e, "INSERT INTO t VALUES (1, 1)");
    let b = "INSERT INTO t VALUES (1, 2) ON CONFLICT (k) DO NOTHING";
    waits(e.execute_in(b, IMPLICIT_TX), "B");
    e.execute_in("ROLLBACK", a).unwrap();
    e.execute_in(b, IMPLICIT_TX).unwrap();
    assert_eq!(cell(&mut e, "SELECT k, n FROM t"), "1:2");
}

#[test]
fn a_waiter_inside_a_transaction_waits_too() {
    let mut e = engine("CREATE TABLE t (k int UNIQUE, n int)");
    let a = holder(&mut e, "INSERT INTO t VALUES (1, 1)");
    let b = e.alloc_tx_id();
    e.execute_in("BEGIN", b).unwrap();
    waits(e.execute_in("INSERT INTO t VALUES (1, 2)", b), "in-tx B");
    e.execute_in("COMMIT", a).unwrap();
    assert!(e.execute_in("INSERT INTO t VALUES (1, 2)", b).is_err());
}

/// A holds 1 and wants 2; B holds 2 and wants 1. PG breaks the cycle by
/// aborting one of them and the other goes on to commit both keys.
#[test]
fn two_transactions_each_holding_the_others_key_is_a_deadlock() {
    let mut e = engine("CREATE TABLE t (k int UNIQUE)");
    let a = holder(&mut e, "INSERT INTO t VALUES (1)");
    let b = holder(&mut e, "INSERT INTO t VALUES (2)");
    waits(e.execute_in("INSERT INTO t VALUES (2)", a), "A wanting 2");
    let closing = e.execute_in("INSERT INTO t VALUES (1)", b);
    assert!(
        matches!(closing, Err(EngineError::LockDeadlock)),
        "the wait that closes the cycle must be a deadlock, got {closing:?}"
    );
    e.execute_in("ROLLBACK", b).unwrap();
    e.execute_in("INSERT INTO t VALUES (2)", a)
        .expect("the survivor proceeds once the victim is gone");
    e.execute_in("COMMIT", a).unwrap();
    assert_eq!(cell(&mut e, "SELECT count(*) FROM t"), "2");
}

/// Keys are computed the way the uniqueness check computes them, so an
/// expression index collides on its expression, not on the raw column.
#[test]
fn an_expression_index_key_is_held_by_its_expression() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (email text)").unwrap();
    e.execute("CREATE UNIQUE INDEX u_lower ON u (LOWER(email))")
        .unwrap();
    let _a = holder(&mut e, "INSERT INTO u VALUES ('A@x.com')");
    waits(
        e.execute_in("INSERT INTO u VALUES ('a@x.com')", IMPLICIT_TX),
        "lower() collision",
    );
}

/// The adjacent shapes that must NOT wait: a different key, a table
/// with no unique rule, and a writer with the same key as its OWN
/// earlier row (which is its own business, decided by the check).
#[test]
fn nothing_waits_when_no_other_transaction_holds_the_key() {
    let mut e = engine("CREATE TABLE t (k int UNIQUE, n int)");
    let _a = holder(&mut e, "INSERT INTO t VALUES (1, 1)");
    e.execute_in("INSERT INTO t VALUES (2, 2)", IMPLICIT_TX)
        .expect("a different key does not wait");

    let mut f = engine("CREATE TABLE p (k int, n int)");
    let _a = holder(&mut f, "INSERT INTO p VALUES (1, 1)");
    f.execute_in("INSERT INTO p VALUES (1, 2)", IMPLICIT_TX)
        .expect("no unique rule, nothing to wait for");

    let mut g = engine("CREATE TABLE t (k int UNIQUE, n int)");
    let own = holder(&mut g, "INSERT INTO t VALUES (1, 1)");
    let r = g.execute_in("INSERT INTO t VALUES (1, 2)", own);
    assert!(
        !matches!(r, Err(EngineError::LockWouldBlock)),
        "a transaction never waits on itself, got {r:?}"
    );
}
