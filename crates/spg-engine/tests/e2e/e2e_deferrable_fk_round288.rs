//! v7.39 (round 288) — `DEFERRABLE INITIALLY DEFERRED` foreign keys.
//!
//! The clauses parsed since v7.17 and were dropped on the floor, so a
//! constraint declared DEFERRABLE behaved as NOT DEFERRABLE. That makes
//! a circular-FK migration impossible to load: whichever table you
//! insert into first violates the other's key, which is exactly the
//! shape pg_dump emits for mutually-referencing tables.
//!
//! The check now moves to COMMIT, and `SET CONSTRAINTS` moves it back.
//!
//! Two things measured rather than assumed:
//!
//!   * `SET CONSTRAINTS ALL IMMEDIATE` runs the pending checks AT THAT
//!     STATEMENT — PG reports the violation there, not at COMMIT.
//!   * a COMMIT that fails this way ENDS the transaction. PG does not
//!     leave the session sitting in an aborted block; the next
//!     statement runs normally.
//!
//! Every expectation was read off live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    spg_engine::eval::value_to_text(&rows[0].values[0])
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}").replace("unsupported: ", ""),
    }
}

fn deferred_fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE dparent (id int primary key)")
        .unwrap();
    e.execute(
        "CREATE TABLE dchild (id int primary key, pid int \
         REFERENCES dparent(id) DEFERRABLE INITIALLY DEFERRED)",
    )
    .unwrap();
    e
}

#[test]
fn a_child_may_precede_its_parent_inside_the_transaction() {
    // The migration shape this exists for.
    let mut e = deferred_fixture();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO dchild VALUES (1, 100)").unwrap();
    e.execute("INSERT INTO dparent VALUES (100)").unwrap();
    e.execute("COMMIT").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM dchild"), "1");
}

#[test]
fn an_unresolved_violation_fails_the_commit() {
    let mut e = deferred_fixture();
    e.execute("BEGIN").unwrap();
    // Accepted here — the check is deferred.
    e.execute("INSERT INTO dchild VALUES (2, 999)").unwrap();
    assert_eq!(
        err(&mut e, "COMMIT"),
        "insert or update on table \"dchild\" violates foreign key \
         constraint \"dchild_pid_fkey\" DETAIL: Key (pid)=(999) is not \
         present in table \"dparent\".",
    );
    // The failed COMMIT ended the transaction — no aborted block.
    assert_eq!(one(&mut e, "SELECT count(*) FROM dchild"), "0");
}

#[test]
fn a_non_deferrable_key_still_fails_at_the_statement() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p2 (id int primary key)").unwrap();
    e.execute("CREATE TABLE c2 (id int primary key, pid int REFERENCES p2(id))")
        .unwrap();
    e.execute("BEGIN").unwrap();
    assert!(e.execute("INSERT INTO c2 VALUES (1, 555)").is_err());
    let _ = e.execute("ROLLBACK");
}

#[test]
fn deferrable_initially_immediate_is_checked_at_the_statement() {
    // DEFERRABLE alone does not defer — the timing keyword does.
    let mut e = Engine::new();
    e.execute("CREATE TABLE p3 (id int primary key)").unwrap();
    e.execute(
        "CREATE TABLE c3 (id int primary key, pid int \
         REFERENCES p3(id) DEFERRABLE INITIALLY IMMEDIATE)",
    )
    .unwrap();
    e.execute("BEGIN").unwrap();
    assert!(e.execute("INSERT INTO c3 VALUES (1, 7)").is_err());
    let _ = e.execute("ROLLBACK");
}

#[test]
fn set_constraints_all_deferred_defers_an_immediate_one() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p4 (id int primary key)").unwrap();
    e.execute(
        "CREATE TABLE c4 (id int primary key, pid int \
         REFERENCES p4(id) DEFERRABLE INITIALLY IMMEDIATE)",
    )
    .unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("SET CONSTRAINTS ALL DEFERRED").unwrap();
    // Now accepted where it errored a moment ago.
    e.execute("INSERT INTO c4 VALUES (1, 7)").unwrap();
    e.execute("INSERT INTO p4 VALUES (7)").unwrap();
    e.execute("COMMIT").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM c4"), "1");
}

#[test]
fn set_constraints_all_immediate_fires_the_pending_check_there() {
    // Not at COMMIT — at this statement. Running the mode change
    // BEFORE the check would empty the set being walked and let the
    // violation reach a successful COMMIT.
    let mut e = deferred_fixture();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO dchild VALUES (1, 42)").unwrap();
    assert_eq!(
        err(&mut e, "SET CONSTRAINTS ALL IMMEDIATE"),
        "insert or update on table \"dchild\" violates foreign key \
         constraint \"dchild_pid_fkey\" DETAIL: Key (pid)=(42) is not \
         present in table \"dparent\".",
    );
    let _ = e.execute("ROLLBACK");
    assert_eq!(one(&mut e, "SELECT count(*) FROM dchild"), "0");
}

#[test]
fn a_rollback_discards_the_deferred_rows() {
    let mut e = deferred_fixture();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO dchild VALUES (2, 43)").unwrap();
    e.execute("ROLLBACK").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM dchild"), "0");
}

#[test]
fn a_row_deleted_later_in_the_transaction_does_not_fail_the_commit() {
    // Why the commit check re-verifies the table rather than replaying
    // a queue of rows: a queued copy of row (3,777) would still be
    // checked after the row itself is gone.
    let mut e = deferred_fixture();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO dchild VALUES (3, 777)").unwrap();
    e.execute("DELETE FROM dchild WHERE id = 3").unwrap();
    e.execute("COMMIT").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM dchild"), "0");
}

#[test]
fn the_timing_survives_a_catalog_round_trip() {
    // FILE_VERSION 79 carries the timing byte; a reload that lost it
    // would enforce immediately and reject the child-first insert.
    let mut e = deferred_fixture();
    let bytes = e.catalog().serialize();
    let mut restored = Engine::restore_envelope(&bytes).expect("reload");
    restored.execute("BEGIN").unwrap();
    restored
        .execute("INSERT INTO dchild VALUES (1, 100)")
        .unwrap();
    restored
        .execute("INSERT INTO dparent VALUES (100)")
        .unwrap();
    restored.execute("COMMIT").unwrap();
    assert_eq!(one(&mut restored, "SELECT count(*) FROM dchild"), "1");
}
