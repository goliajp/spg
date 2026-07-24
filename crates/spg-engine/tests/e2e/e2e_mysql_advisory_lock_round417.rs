//! read01 round 417 (MySQL differential) — advisory lock family
//! (`GET_LOCK` / `RELEASE_LOCK` / `IS_FREE_LOCK` / `IS_USED_LOCK` /
//! `RELEASE_ALL_LOCKS`).
//!
//! Distributed-lock pattern used by Bull / Sidekiq / migration tooling
//! and hand-rolled leader elections. SPG errored `function get_lock(text,
//! integer) does not exist` at every entry. The names now route to the
//! same shared advisory-lock registry PG's `pg_advisory_lock` uses,
//! hashing the string name with FNV-1a-64. A PG session keeps the errors.
//!
//! Every semantic expectation is copied from a MariaDB 11 run. The
//! `IS_USED_LOCK` value is intentionally NOT pinned to an exact number —
//! MariaDB returns its connection-id, SPG returns its session-id (both
//! valid owner ids in their own database's id space); the test only
//! checks the shape (non-NULL when held, NULL when free).
//!
//! `timeout` is not honoured — SPG is a single-process engine, so an
//! uncontended lock is always available and a contended one can't be
//! released by a peer while we wait. The wait is a no-op either way.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone().into_owned(),
        other => panic!("{other:?}"),
    }
}

/// GET_LOCK returns 1 the first time and re-entering returns 1 too.
#[test]
fn get_lock_takes_and_reenters() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT GET_LOCK('r417_a', 0)"), Value::Int(1));
    // A held lock is not free.
    assert_eq!(one(&mut e, "SELECT IS_FREE_LOCK('r417_a')"), Value::Int(0));
    // Re-entry succeeds, bumping the depth.
    assert_eq!(one(&mut e, "SELECT GET_LOCK('r417_a', 0)"), Value::Int(1));
}

/// A NULL name yields NULL for every arm.
#[test]
fn null_name_null_result() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT GET_LOCK(NULL, 0)"), Value::Null);
    assert_eq!(one(&mut e, "SELECT RELEASE_LOCK(NULL)"), Value::Null);
    assert_eq!(one(&mut e, "SELECT IS_FREE_LOCK(NULL)"), Value::Null);
    assert_eq!(one(&mut e, "SELECT IS_USED_LOCK(NULL)"), Value::Null);
}

/// RELEASE_LOCK: 1 = released, 0 = held by someone else, NULL = no lock
/// with that name existed anywhere.
#[test]
fn release_lock_return_shape() {
    let mut e = mysql();
    // Nobody has 'r417_never' -> NULL.
    assert_eq!(one(&mut e, "SELECT RELEASE_LOCK('r417_never')"), Value::Null);
    // Take + release + one more release for a not-held name -> NULL.
    e.execute("SELECT GET_LOCK('r417_b', 0)").unwrap();
    assert_eq!(one(&mut e, "SELECT RELEASE_LOCK('r417_b')"), Value::Int(1));
    assert_eq!(one(&mut e, "SELECT RELEASE_LOCK('r417_b')"), Value::Null);
}

/// IS_FREE_LOCK / IS_USED_LOCK for a lock nobody has ever taken.
#[test]
fn probe_unknown_lock() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT IS_FREE_LOCK('r417_ghost')"), Value::Int(1));
    assert_eq!(one(&mut e, "SELECT IS_USED_LOCK('r417_ghost')"), Value::Null);
}

/// IS_USED_LOCK when held returns a non-NULL owner id (MariaDB's exact
/// connection-id vs SPG's session-id differs; we only check the shape).
#[test]
fn is_used_lock_when_held_is_non_null() {
    let mut e = mysql();
    e.execute("SELECT GET_LOCK('r417_c', 0)").unwrap();
    let v = one(&mut e, "SELECT IS_USED_LOCK('r417_c')");
    assert!(
        matches!(v, Value::BigInt(_) | Value::Int(_)),
        "held lock should surface an owner id, got {v:?}"
    );
}

/// RELEASE_ALL_LOCKS returns the released count and frees them all.
#[test]
fn release_all_locks_counts_and_frees() {
    let mut e = mysql();
    e.execute("SELECT GET_LOCK('a1', 0)").unwrap();
    e.execute("SELECT GET_LOCK('a2', 0)").unwrap();
    assert_eq!(one(&mut e, "SELECT RELEASE_ALL_LOCKS()"), Value::Int(2));
    // Both are free again.
    assert_eq!(one(&mut e, "SELECT IS_FREE_LOCK('a1')"), Value::Int(1));
    assert_eq!(one(&mut e, "SELECT IS_FREE_LOCK('a2')"), Value::Int(1));
}

/// A PostgreSQL session has no MySQL lock names and rejects them.
#[test]
fn postgres_rejects() {
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT get_lock('x', 0)").is_err(),
        "PG has no GET_LOCK(text, int)"
    );
    assert!(
        e.execute("SELECT release_lock('x')").is_err(),
        "PG has no RELEASE_LOCK(text)"
    );
}
