//! v7.39 (round 279) — per-connection session state, and advisory
//! locks that actually exclude.
//!
//! The server runs ONE shared `Engine` behind a RwLock, built once at
//! startup — not one per connection. Everything the engine called
//! "session state" was therefore process-wide and leaked between
//! clients. This pins the three that matter and the advisory-lock
//! registry that depends on knowing who is asking.
//!
//! Advisory-lock expectations were read off live PG 18.4 in a single
//! session; the cross-session ones are PG's documented semantics
//! exercised here through `set_current_session`, which is what the
//! server calls for each connection.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    assert_eq!(rows.len(), 1, "{sql}");
    match &rows[0].values[0] {
        spg_storage::Value::Null => String::new(),
        other => spg_engine::eval::value_to_text(other),
    }
}

#[test]
fn advisory_locks_are_re_entrant_and_counted() {
    let mut e = Engine::new();
    // Live PG 18.4, in order: t t t t f f
    assert_eq!(one(&mut e, "SELECT pg_try_advisory_lock(42)"), "true");
    assert_eq!(one(&mut e, "SELECT pg_try_advisory_lock(42)"), "true");
    assert_eq!(one(&mut e, "SELECT pg_advisory_unlock(42)"), "true");
    assert_eq!(one(&mut e, "SELECT pg_advisory_unlock(42)"), "true");
    // Both levels released; a third unlock owns nothing.
    assert_eq!(one(&mut e, "SELECT pg_advisory_unlock(42)"), "false");
    assert_eq!(one(&mut e, "SELECT pg_advisory_unlock(999)"), "false");
}

#[test]
fn the_two_int_key_addresses_the_same_space() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_try_advisory_lock(1,2)"), "true");
    assert_eq!(one(&mut e, "SELECT pg_advisory_unlock(1,2)"), "true");
}

#[test]
fn unlock_all_releases_this_sessions_locks() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_try_advisory_lock(7)"), "true");
    assert_eq!(one(&mut e, "SELECT pg_advisory_unlock_all()"), "");
    assert_eq!(one(&mut e, "SELECT pg_advisory_unlock(7)"), "false");
}

#[test]
fn a_lock_held_by_another_session_is_refused() {
    // The whole point of an advisory lock. The old stub answered true
    // unconditionally, so two sqlx migrators both entered their
    // check-then-apply section — SPG's per-STATEMENT write lock does
    // not stop two connections interleaving BETWEEN statements.
    let mut e = Engine::new();
    e.set_current_session(1);
    assert_eq!(one(&mut e, "SELECT pg_try_advisory_lock(100)"), "true");
    e.set_current_session(2);
    assert_eq!(one(&mut e, "SELECT pg_try_advisory_lock(100)"), "false");
    // Session 2 cannot release what it does not hold.
    assert_eq!(one(&mut e, "SELECT pg_advisory_unlock(100)"), "false");
    // Session 1 releases; now session 2 can take it.
    e.set_current_session(1);
    assert_eq!(one(&mut e, "SELECT pg_advisory_unlock(100)"), "true");
    e.set_current_session(2);
    assert_eq!(one(&mut e, "SELECT pg_try_advisory_lock(100)"), "true");
}

#[test]
fn a_disconnect_releases_the_locks_it_held() {
    // PG releases advisory locks at backend exit; without this a
    // crashed client would hold one forever in the shared engine.
    let mut e = Engine::new();
    e.set_current_session(1);
    assert_eq!(one(&mut e, "SELECT pg_try_advisory_lock(55)"), "true");
    e.end_session(1);
    e.set_current_session(2);
    assert_eq!(one(&mut e, "SELECT pg_try_advisory_lock(55)"), "true");
}

#[test]
fn prepared_statements_do_not_leak_between_sessions() {
    // Round 277 added these to the shared engine and its pin claimed
    // they were "session-scoped exactly as in PG". They were not:
    // every connection saw every other connection's, and one client's
    // DEALLOCATE ALL cleared another's.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int)").unwrap();
    e.set_current_session(1);
    e.execute("PREPARE mine AS SELECT 1").unwrap();
    e.set_current_session(2);
    let msg = format!("{:?}", e.execute("EXECUTE mine").unwrap_err());
    assert!(msg.contains("does not exist"), "{msg}");
    // Session 2 may use the same NAME for a different statement.
    e.execute("PREPARE mine AS SELECT 2").unwrap();
    assert_eq!(one(&mut e, "EXECUTE mine"), "2");
    // And session 1 still has its own.
    e.set_current_session(1);
    assert_eq!(one(&mut e, "EXECUTE mine"), "1");
}

#[test]
fn deallocate_all_only_clears_the_calling_session() {
    let mut e = Engine::new();
    e.set_current_session(1);
    e.execute("PREPARE a AS SELECT 1").unwrap();
    e.set_current_session(2);
    e.execute("PREPARE b AS SELECT 2").unwrap();
    e.execute("DEALLOCATE ALL").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM pg_prepared_statements"), "0");
    e.set_current_session(1);
    assert_eq!(one(&mut e, "SELECT count(*) FROM pg_prepared_statements"), "1");
    assert_eq!(one(&mut e, "EXECUTE a"), "1");
}

#[test]
fn a_guc_override_does_not_leak_between_sessions() {
    let mut e = Engine::new();
    e.set_current_session(1);
    e.execute("SET work_mem = '64MB'").unwrap();
    assert_eq!(one(&mut e, "SHOW work_mem"), "64MB");
    e.set_current_session(2);
    // Session 2 sees the default, not session 1's override.
    assert_ne!(one(&mut e, "SHOW work_mem"), "64MB");
}
