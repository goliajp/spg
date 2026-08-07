//! v7.39 (round 552) — SERIALIZABLE was Snapshot Isolation.
//!
//! The audit's phase 6 is "verification debt — not undone, unproven",
//! and its first entry is SSI conflict detection, wanting a concurrent
//! write-skew case. Measured over the wire against PG18, the classic
//! one destroyed its invariant:
//!
//!     both doctors go off call, each having seen the other on call
//!     PG18  T2 aborts 40001, one stays on call
//!     SPG   both commit, NONE stays on call
//!
//! an outcome no serial order can produce, under a transaction that
//! declared SERIALIZABLE.
//!
//! What SPG did have was the write-WRITE half: two transactions
//! updating the same row raise `could not serialize access due to
//! concurrent update`, and the surviving value matches PG's. So the
//! level was Snapshot Isolation exactly — first-committer-wins, no
//! read/write antidependency.
//!
//! The missing half is now table-granularity SIREAD: a SERIALIZABLE
//! transaction records the tables it reads, and aborts at COMMIT if any
//! of them was written by a transaction that committed after its
//! snapshot. That is the coarse end of PG's own mechanism — what PG
//! itself falls back to when its per-tuple lock memory runs out — so
//! SPG aborts some transactions PG would let through and never lets
//! through one PG would abort.
//!
//! The level had to move onto the TRANSACTION: `current_isolation_level`
//! is one field for the whole engine and is not in the per-session bag,
//! so with two connections open one transaction's COMMIT reset it under
//! the other's feet — the shared-engine leak rounds 279 and 283 chased
//! through session state and advisory locks.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult, TxId};

fn rows(e: &mut Engine, sql: &str, tx: TxId) -> Vec<String> {
    match e
        .execute_in(sql, tx)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
    {
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

const IMPLICIT: TxId = TxId(0);

fn oncall_engine() -> Engine {
    let mut e = Engine::new();
    e.execute_in(
        "CREATE TABLE oncall (name TEXT PRIMARY KEY, on_call BOOLEAN)",
        IMPLICIT,
    )
    .unwrap();
    e.execute_in(
        "INSERT INTO oncall VALUES ('alice', true), ('bob', true)",
        IMPLICIT,
    )
    .unwrap();
    e
}

/// The classic write skew: one of the two must not commit.
#[test]
fn round552_write_skew_is_refused() {
    let mut e = oncall_engine();
    let (t1, t2) = (TxId(11), TxId(12));
    e.execute_in("BEGIN ISOLATION LEVEL SERIALIZABLE", t1)
        .unwrap();
    e.execute_in("BEGIN ISOLATION LEVEL SERIALIZABLE", t2)
        .unwrap();
    // Each sees both on call.
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM oncall WHERE on_call", t1),
        vec!["2"]
    );
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM oncall WHERE on_call", t2),
        vec!["2"]
    );
    e.execute_in("UPDATE oncall SET on_call = false WHERE name = 'alice'", t1)
        .unwrap();
    e.execute_in("UPDATE oncall SET on_call = false WHERE name = 'bob'", t2)
        .unwrap();
    e.execute_in("COMMIT", t1).unwrap();
    let err = format!(
        "{}",
        e.execute_in("COMMIT", t2)
            .expect_err("the second commit completes a cycle no serial order allows")
    );
    assert!(
        err.contains("could not serialize access"),
        "message was {err}"
    );
    // The invariant holds: somebody is still on call.
    assert_eq!(
        rows(
            &mut e,
            "SELECT count(*) FROM oncall WHERE on_call",
            IMPLICIT
        ),
        vec!["1"]
    );
}

/// The write-WRITE half, which already worked, still does.
#[test]
fn round552_same_row_conflict_still_caught() {
    let mut e = Engine::new();
    e.execute_in("CREATE TABLE cw (k INT PRIMARY KEY, n INT)", IMPLICIT)
        .unwrap();
    e.execute_in("INSERT INTO cw VALUES (1, 10)", IMPLICIT)
        .unwrap();
    let (t1, t2) = (TxId(21), TxId(22));
    e.execute_in("BEGIN ISOLATION LEVEL SERIALIZABLE", t1)
        .unwrap();
    e.execute_in("BEGIN ISOLATION LEVEL SERIALIZABLE", t2)
        .unwrap();
    e.execute_in("UPDATE cw SET n = n + 1 WHERE k = 1", t1)
        .unwrap();
    e.execute_in("COMMIT", t1).unwrap();
    // PG raises here or at commit; either way the second one loses.
    let second = e
        .execute_in("UPDATE cw SET n = n + 100 WHERE k = 1", t2)
        .and_then(|_| e.execute_in("COMMIT", t2));
    assert!(second.is_err(), "the loser must not commit");
    assert_eq!(
        rows(&mut e, "SELECT n FROM cw WHERE k = 1", IMPLICIT),
        vec!["11"]
    );
}

/// Two SERIALIZABLE transactions that share nothing both commit — the
/// check must not abort transactions with no dependency between them.
#[test]
fn round552_disjoint_transactions_both_commit() {
    let mut e = Engine::new();
    e.execute_in("CREATE TABLE a (k INT)", IMPLICIT).unwrap();
    e.execute_in("CREATE TABLE b (k INT)", IMPLICIT).unwrap();
    e.execute_in("INSERT INTO a VALUES (1)", IMPLICIT).unwrap();
    e.execute_in("INSERT INTO b VALUES (1)", IMPLICIT).unwrap();
    let (t1, t2) = (TxId(31), TxId(32));
    e.execute_in("BEGIN ISOLATION LEVEL SERIALIZABLE", t1)
        .unwrap();
    e.execute_in("BEGIN ISOLATION LEVEL SERIALIZABLE", t2)
        .unwrap();
    rows(&mut e, "SELECT count(*) FROM a", t1);
    rows(&mut e, "SELECT count(*) FROM b", t2);
    e.execute_in("INSERT INTO a VALUES (2)", t1).unwrap();
    e.execute_in("INSERT INTO b VALUES (2)", t2).unwrap();
    e.execute_in("COMMIT", t1).unwrap();
    e.execute_in("COMMIT", t2)
        .expect("nothing links these two transactions");
}

/// A REPEATABLE READ transaction keeps its old behaviour — the check is
/// SERIALIZABLE's alone, as it is in PG.
#[test]
fn round552_repeatable_read_is_unchanged() {
    let mut e = oncall_engine();
    let (t1, t2) = (TxId(41), TxId(42));
    e.execute_in("BEGIN ISOLATION LEVEL REPEATABLE READ", t1)
        .unwrap();
    e.execute_in("BEGIN ISOLATION LEVEL REPEATABLE READ", t2)
        .unwrap();
    rows(&mut e, "SELECT count(*) FROM oncall WHERE on_call", t1);
    rows(&mut e, "SELECT count(*) FROM oncall WHERE on_call", t2);
    e.execute_in("UPDATE oncall SET on_call = false WHERE name = 'alice'", t1)
        .unwrap();
    e.execute_in("UPDATE oncall SET on_call = false WHERE name = 'bob'", t2)
        .unwrap();
    e.execute_in("COMMIT", t1).unwrap();
    // PG allows write skew at REPEATABLE READ; so does SPG.
    e.execute_in("COMMIT", t2)
        .expect("REPEATABLE READ permits write skew, in PG too");
}
