//! 9.0.0 — `pg_has_role` answered three things wrong.
//!
//! Measured on PostgreSQL 18.6, as the superuser and again after
//! `SET ROLE alice`:
//!
//! | call | superuser | alice | SPG before |
//! |---|---|---|---|
//! | `pg_has_role('pg_monitor','USAGE')` | `t` | `f` | `f` for both |
//! | `pg_has_role(999999::oid,'USAGE')` | `t` | `f` | `t` for both |
//! | `pg_has_role('no_such_role','USAGE')` | ERROR | ERROR | `f` |
//!
//! The oid spelling returned `true` for ANY oid because a non-text
//! argument fell through to a bare `return true`. A superuser was told
//! it is a member of nothing. And a role name that is a typo answered
//! `false`, which reads as a real "you are not a member".
//!
//! PostgreSQL's `information_schema.enabled_roles`,
//! `applicable_roles`, `udt_privileges` and `parameters` are all built
//! on this function.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    spg_engine::eval::value_to_text(
        rows.first()
            .expect("one row")
            .values
            .first()
            .expect("one column"),
    )
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE ROLE alice",
        "CREATE ROLE readers",
        "GRANT readers TO alice",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

#[test]
fn a_superuser_holds_every_role() {
    let mut e = seeded();
    assert_eq!(
        one(&mut e, "SELECT pg_has_role('pg_monitor','USAGE')::text"),
        "true"
    );
    assert_eq!(
        one(&mut e, "SELECT pg_has_role('alice','USAGE')::text"),
        "true"
    );
}

#[test]
fn a_role_name_that_does_not_exist_raises() {
    let mut e = seeded();
    let err = e
        .execute("SELECT pg_has_role('no_such_role_xyz','USAGE')")
        .expect_err("PG raises for a name no role carries");
    assert!(
        format!("{err:?}").contains("no_such_role_xyz")
            && format!("{err:?}").contains("does not exist"),
        "{err:?}"
    );
}

#[test]
fn a_non_member_gets_false_and_a_member_gets_true() {
    let mut e = seeded();
    // The three-argument spelling names the member, so this asks about
    // alice rather than about the session's superuser.
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_has_role('alice','readers','USAGE')::text"
        ),
        "true"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_has_role('alice','pg_monitor','USAGE')::text"
        ),
        "false"
    );
}

#[test]
fn an_oid_no_role_carries_is_false_for_a_non_superuser() {
    let mut e = seeded();
    // It used to answer `true` for every oid, superuser or not, because
    // a non-text argument never reached the membership question.
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_has_role('alice', 999999::oid, 'USAGE')::text"
        ),
        "false"
    );
    // And an oid that DOES name a role answers the membership question.
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_has_role('alice', (SELECT oid FROM pg_roles WHERE rolname='readers'), 'USAGE')::text"
        ),
        "true"
    );
}
