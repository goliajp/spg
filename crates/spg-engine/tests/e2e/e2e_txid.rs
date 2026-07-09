//! v7.38 (T24 / U22) — the `txid_*` / `pg_*_xact_id` / `pg_xact_status` family
//! reports real transaction ids and a real three-state status. SPG's writer
//! versions ARE its transaction ids, so no separate xid counter exists — that
//! bridge is what U22 was waiting on. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn text(e: &mut Engine, sql: &str) -> String {
    match one(e, sql) {
        Value::Text(s) => s.to_string(),
        other => panic!("expected text, got {other:?}"),
    }
}

fn bigint(e: &mut Engine, sql: &str) -> i64 {
    match one(e, sql) {
        Value::BigInt(n) => n,
        other => panic!("expected bigint, got {other:?}"),
    }
}

#[test]
fn txid_current_is_a_real_id_not_a_stub() {
    // It used to be the constant 1.
    let mut e = Engine::new();
    assert!(bigint(&mut e, "SELECT txid_current()") > 1);
    // Stable within one statement: PG's `SELECT txid_current(), txid_current()`
    // agrees with itself because the id is assigned once per transaction.
    assert_eq!(
        one(&mut e, "SELECT txid_current() = txid_current()"),
        Value::Bool(true)
    );
    // pg_current_xact_id is the modern spelling of the same thing.
    assert!(bigint(&mut e, "SELECT pg_current_xact_id()") > 1);
}

#[test]
fn if_assigned_is_null_without_an_assigned_id() {
    // PG returns NULL for a read-only autocommit statement that has not been
    // assigned an id.
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT pg_current_xact_id_if_assigned()"),
        Value::Null
    );
    assert_eq!(
        one(&mut e, "SELECT txid_current_if_assigned()"),
        Value::Null
    );
}

#[test]
fn txid_is_stable_across_statements_of_a_transaction() {
    let mut e = Engine::new();
    e.execute("BEGIN").unwrap();
    let a = bigint(&mut e, "SELECT txid_current()");
    let b = bigint(&mut e, "SELECT txid_current()");
    assert_eq!(a, b, "a transaction keeps one id across its statements");
    // SPG allocates the id at BEGIN (concurrent readers need it to exclude the
    // transaction from their snapshots), so it is already assigned here. PG
    // defers assignment to the first write, and would return NULL until then.
    assert_eq!(
        one(&mut e, "SELECT pg_current_xact_id_if_assigned()"),
        Value::BigInt(a)
    );
    e.execute("COMMIT").unwrap();
}

#[test]
fn xact_status_reports_all_three_states() {
    // This is U22: the stub always answered "committed".
    let mut e = Engine::new();

    // A transaction still in flight sees itself as in progress.
    e.execute("BEGIN").unwrap();
    let live = bigint(&mut e, "SELECT txid_current()");
    assert_eq!(text(&mut e, "SELECT txid_status(txid_current())"), "in progress");
    e.execute("COMMIT").unwrap();

    // …and as committed once it lands.
    assert_eq!(
        text(&mut e, &format!("SELECT txid_status({live})")),
        "committed"
    );

    // A rolled-back transaction is aborted, not committed.
    e.execute("BEGIN").unwrap();
    let dead = bigint(&mut e, "SELECT txid_current()");
    e.execute("ROLLBACK").unwrap();
    assert_eq!(
        text(&mut e, &format!("SELECT txid_status({dead})")),
        "aborted"
    );

    // pg_xact_status is the modern spelling.
    assert_eq!(
        text(&mut e, &format!("SELECT pg_xact_status({live})")),
        "committed"
    );
}

#[test]
fn xact_status_rejects_an_id_never_handed_out() {
    // PG: "transaction ID N is in the future".
    let mut e = Engine::new();
    let err = e
        .execute("SELECT txid_status(txid_current() + 1000)")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("in the future"), "unexpected error: {msg}");

    // NULL in, NULL out.
    assert_eq!(one(&mut e, "SELECT txid_status(NULL)"), Value::Null);
}
