//! Round 696 — four statements that performed nothing, and did not check
//! what they were performing nothing ON.
//!
//! Round 695's F31 sweep measured these against PG18: `LOCK TABLE nosuch`,
//! `DROP OWNED BY nosuchrole`, `REASSIGN OWNED BY nosuchrole` and `SECURITY
//! LABEL` were all ACCEPTED here and all refused there. Every one was being
//! swallowed whole by the parser's dump-noise list, so there was never a
//! name to check.
//!
//! SPG still performs nothing for any of them — there is no role-owner
//! model, no label provider, and the engine holds a process-wide write lock
//! so an explicit LOCK has no effect. What changed is that it no longer
//! reports understanding about an object that is not there.
//!
//! They share one `Statement::ValidateOnly` because they share one rule:
//! resolve the name, refuse if absent, otherwise no-op. Four variants would
//! be four places for that rule to drift — which is exactly what the round
//! found underneath.
//!
//! ## What checking the names uncovered
//!
//! `DROP OWNED BY bench` was refused for `bench`, the CONNECTED USER. The
//! authenticated wire identity was never registered as a role: `current_user`
//! reported it, and `pg_roles` did not list it, `'bench'::regrole` said it
//! did not exist, and `SET ROLE bench` refused the role the session was
//! ALREADY running as.
//!
//! That is the same class round 652 closed for `postgres` — a role predicate
//! disagreeing with the catalogue it is supposed to reflect — and it was
//! missed then because nothing asked. Three separate predicates had grown up
//! (`role_exists`, an inline `users.any(…) || postgres` in the ALTER ROLE
//! path, and `acl_check_role_exists`); they answer through one now.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
    assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql}: {r:?}");
}

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!(
        "{}",
        e.execute(sql)
            .expect_err(&format!("PG18 refuses this: {sql}"))
    )
}

#[test]
fn round696_lock_table_refuses_a_missing_relation() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE lk(i INT)").unwrap();
    // Every legitimate spelling still passes.
    ok(&mut e, "LOCK TABLE lk");
    ok(&mut e, "LOCK lk");
    ok(&mut e, "LOCK TABLE lk IN ACCESS EXCLUSIVE MODE");
    ok(&mut e, "LOCK TABLE lk NOWAIT");
    ok(&mut e, "LOCK TABLE lk, lk");
    assert!(err_of(&mut e, "LOCK TABLE nosuch696").contains("nosuch696"));
}

/// MySQL's `LOCK TABLES … READ|WRITE` is a different statement that happens
/// to start with the same word, and a mysqldump bracket names tables it is
/// about to create. It keeps the old no-op.
#[test]
fn round696_mysql_lock_tables_is_still_a_bracket() {
    let mut e = Engine::new();
    ok(&mut e, "LOCK TABLES notyetcreated696 WRITE");
    ok(&mut e, "UNLOCK TABLES");
}

#[test]
fn round696_owned_by_refuses_a_missing_role() {
    let mut e = Engine::new();
    assert!(err_of(&mut e, "DROP OWNED BY nosuch696").contains("nosuch696"));
    assert!(err_of(&mut e, "REASSIGN OWNED BY nosuch696 TO postgres").contains("nosuch696"));
    // The bootstrap role, and the session's own, are accepted.
    ok(&mut e, "DROP OWNED BY postgres");
    ok(&mut e, "REASSIGN OWNED BY postgres TO postgres");
}

/// PG18 refuses this whatever it names, because no label provider is
/// loaded. SPG has none either, so the refusal is the honest answer rather
/// than a stand-in for one.
#[test]
fn round696_security_label_is_refused_unconditionally() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE sl(i INT)").unwrap();
    for sql in [
        "SECURITY LABEL ON TABLE sl IS 'x'",
        "SECURITY LABEL ON TABLE nosuch696 IS 'x'",
        "SECURITY LABEL FOR selinux ON TABLE sl IS 'x'",
    ] {
        assert!(
            err_of(&mut e, sql).contains("no security label providers have been loaded"),
            "{sql}"
        );
    }
}

/// The session's own identity is a role. `current_user` always said so;
/// nothing else did.
#[test]
fn round696_the_session_identity_is_a_role() {
    let mut e = Engine::new();
    let me = match e.execute("SELECT current_user").unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    };
    assert!(!me.is_empty());
    ok(&mut e, &format!("SET ROLE {me}"));
    ok(&mut e, "SET ROLE NONE");
    ok(&mut e, &format!("DROP OWNED BY {me}"));
    // And it is visible where a role should be visible.
    let listed = match e.execute("SELECT rolname FROM pg_roles").unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert!(
        listed.contains(&me),
        "pg_roles should list {me}: {listed:?}"
    );
    // A name that is not a role still is not one.
    assert!(err_of(&mut e, "SET ROLE nosuch696").contains("nosuch696"));
}
