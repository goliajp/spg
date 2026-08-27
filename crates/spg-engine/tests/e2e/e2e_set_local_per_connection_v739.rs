//! v7.39 — `SET LOCAL` belongs to the connection that wrote it.
//!
//! The server runs ONE shared `Engine`: each connection gets its own
//! `SessionBag` (via `set_current_session`) and its own `tx_id` (open
//! or not). The `SET LOCAL` undo log was neither — one `Vec` for the
//! whole Engine, guarded by `in_transaction()`, which is true when ANY
//! connection has a transaction open.
//!
//! Measured on a running server before the fix, two connections, `app.k`
//! written only ever by B:
//!
//! ```text
//! A 起始      <unset>
//! A 提交后    Bvalue     <- A committed its own transaction and came
//!                           out holding a value only B had written
//! B 提交后    Blocal     <- B is autocommit; its LOCAL value stayed
//! ```
//!
//! In the shape applications actually write this — RLS and multi-tenancy
//! both do `set_config('app.tenant_id', …, true)` once per request —
//! that is one request's tenant id surviving into the next, and one
//! connection reading a tenant id set by another.
//!
//! Two sessions and two slots are what it takes: the 6,622 tests in this
//! harness run one session on the implicit slot, where neither half of
//! the defect exists.

use spg_engine::{Engine, IMPLICIT_TX, QueryResult, TxId};

/// What THIS connection reads back, as the application would ask.
fn guc(e: &mut Engine, session: u32, tx: TxId, name: &str) -> String {
    e.set_current_session(session);
    let sql = alloc_sql(name);
    let r = e
        .execute_in(&sql, tx)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    match &rows[0].values[0] {
        spg_storage::Value::Null => "<unset>".to_string(),
        other => spg_engine::eval::value_to_text(other),
    }
}

fn alloc_sql(name: &str) -> String {
    format!("SELECT current_setting('{name}', true)")
}

fn run(e: &mut Engine, session: u32, tx: TxId, sql: &str) {
    e.set_current_session(session);
    e.execute_in(sql, tx)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn a_set_local_on_one_connection_stays_off_another() {
    let mut e = Engine::new();
    let a_tx = e.alloc_tx_id();
    let b_tx = e.alloc_tx_id();

    // B writes a session value of its own, then opens nothing.
    run(&mut e, 2, b_tx, "SET app.k = 'Bvalue'");
    assert_eq!(guc(&mut e, 1, a_tx, "app.k"), "<unset>", "A starts clean");

    // A opens a transaction. B, still autocommit, writes a LOCAL value —
    // this is where the global witness used to say "we are in a
    // transaction" because A is.
    run(&mut e, 1, a_tx, "BEGIN");
    run(&mut e, 2, b_tx, "SET LOCAL app.k = 'Blocal'");
    run(&mut e, 1, a_tx, "COMMIT");

    // A never wrote app.k and must not have acquired one.
    assert_eq!(
        guc(&mut e, 1, a_tx, "app.k"),
        "<unset>",
        "A committed its own transaction and must not inherit B's value"
    );
    // B keeps what B set with plain SET; the LOCAL outside a transaction
    // block leaves nothing behind, which is PG's rule.
    assert_eq!(
        guc(&mut e, 2, b_tx, "app.k"),
        "Bvalue",
        "B's LOCAL was not inside a transaction block, so it does not last"
    );
}

#[test]
fn an_unrelated_open_transaction_does_not_change_what_set_local_does() {
    // The same two statements on the same connection, run twice: once
    // alone, once while a different connection holds a transaction. The
    // answer must not depend on what the other connection is doing.
    let answer = |others_tx_open: bool| -> String {
        let mut e = Engine::new();
        let a_tx = e.alloc_tx_id();
        let b_tx = e.alloc_tx_id();
        if others_tx_open {
            run(&mut e, 1, a_tx, "BEGIN");
        }
        run(&mut e, 2, b_tx, "SET LOCAL app.b = 'B'");
        guc(&mut e, 2, b_tx, "app.b")
    };
    assert_eq!(
        answer(false),
        answer(true),
        "a second connection's transaction changed this connection's answer"
    );
    assert_eq!(answer(false), "<unset>", "and the answer is PG's");
}

#[test]
fn inside_its_own_transaction_set_local_still_applies_and_still_reverts() {
    // Without this, "leaks nothing" is satisfied by a SET LOCAL that
    // does nothing at all.
    let mut e = Engine::new();
    let tx = e.alloc_tx_id();
    run(&mut e, 1, tx, "SET app.t = 'session'");
    run(&mut e, 1, tx, "BEGIN");
    run(&mut e, 1, tx, "SET LOCAL app.t = 'local'");
    assert_eq!(
        guc(&mut e, 1, tx, "app.t"),
        "local",
        "SET LOCAL applies for the rest of the transaction"
    );
    run(&mut e, 1, tx, "COMMIT");
    assert_eq!(
        guc(&mut e, 1, tx, "app.t"),
        "session",
        "and the pre-transaction value comes back at COMMIT"
    );
}

#[test]
fn one_connections_commit_does_not_unwind_anothers_savepoint_marks() {
    // The savepoint→undo-depth marks were a global Vec too, so a
    // ROLLBACK TO on one connection read another's depths.
    let mut e = Engine::new();
    let a_tx = e.alloc_tx_id();
    let b_tx = e.alloc_tx_id();

    run(&mut e, 1, a_tx, "SET app.s = 'A0'");
    run(&mut e, 1, a_tx, "BEGIN");
    run(&mut e, 1, a_tx, "SAVEPOINT sp");
    run(&mut e, 1, a_tx, "SET LOCAL app.s = 'A1'");

    // B opens its own transaction and does its own local writes in
    // between, pushing entries that used to land in A's log.
    run(&mut e, 2, b_tx, "BEGIN");
    run(&mut e, 2, b_tx, "SET LOCAL app.s = 'B1'");

    run(&mut e, 1, a_tx, "ROLLBACK TO SAVEPOINT sp");
    assert_eq!(
        guc(&mut e, 1, a_tx, "app.s"),
        "A0",
        "A's ROLLBACK TO must unwind A's own local writes"
    );
    assert_eq!(
        guc(&mut e, 2, b_tx, "app.s"),
        "B1",
        "and must leave B's transaction untouched"
    );
    run(&mut e, 1, a_tx, "COMMIT");
    run(&mut e, 2, b_tx, "COMMIT");
}

#[test]
fn the_implicit_slot_still_behaves() {
    // Every other test in this harness drives IMPLICIT_TX; it has no
    // persistent slot, so a SET LOCAL on it leaves nothing.
    let mut e = Engine::new();
    run(&mut e, 1, IMPLICIT_TX, "SET app.i = 'kept'");
    run(&mut e, 1, IMPLICIT_TX, "SET LOCAL app.i = 'gone'");
    assert_eq!(guc(&mut e, 1, IMPLICIT_TX, "app.i"), "kept");
}

#[test]
fn one_connections_commit_leaves_anothers_open_transaction_alone() {
    // The witness fix alone does not reach this: with it, an autocommit
    // connection writes nothing to any log, so nothing of its can be
    // drained. The undo log being ONE Vec for the Engine only shows when
    // both connections have a transaction open and one of them ends —
    // its COMMIT popped to depth zero, which meant everybody's entries,
    // replayed into whichever session happened to be current.
    let mut e = Engine::new();
    let a_tx = e.alloc_tx_id();
    let b_tx = e.alloc_tx_id();

    run(&mut e, 1, a_tx, "SET app.c = 'A0'");
    run(&mut e, 2, b_tx, "SET app.c = 'B0'");
    run(&mut e, 1, a_tx, "BEGIN");
    run(&mut e, 2, b_tx, "BEGIN");
    run(&mut e, 1, a_tx, "SET LOCAL app.c = 'Alocal'");
    run(&mut e, 2, b_tx, "SET LOCAL app.c = 'Blocal'");

    run(&mut e, 1, a_tx, "COMMIT");
    assert_eq!(
        guc(&mut e, 1, a_tx, "app.c"),
        "A0",
        "A's own local write reverts at A's COMMIT"
    );
    assert_eq!(
        guc(&mut e, 2, b_tx, "app.c"),
        "Blocal",
        "B's transaction is still open, so B's local write still stands"
    );

    run(&mut e, 2, b_tx, "COMMIT");
    assert_eq!(
        guc(&mut e, 2, b_tx, "app.c"),
        "B0",
        "and it reverts when B's own transaction ends"
    );
}
