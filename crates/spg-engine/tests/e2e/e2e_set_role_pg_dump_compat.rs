//! v7.37.17 (17.6 sibling) — SET ROLE pg_dump-compat.
//!
//! v7.39 (read01 round 58) — roles are REAL now, so `SET ROLE` validates like
//! PG: a name that is not a role is an error, not a silent no-op that leaves
//! the session in a role holding nothing. The two built-in superusers — SPG's
//! `admin` login and the `postgres` bootstrap row `pg_roles` has always
//! advertised — are roles, so the dump spellings below still pass.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn set_role_named_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "SET ROLE postgres");
    ddl(&mut e, "CREATE ROLE alice LOGIN PASSWORD 'x'");
    ddl(&mut e, "SET ROLE alice");
}

#[test]
fn set_role_to_an_unknown_role_is_an_error() {
    let mut e = Engine::new();
    assert_eq!(
        format!("{}", e.execute("SET ROLE nosuchrole").unwrap_err()),
        "unsupported: role \"nosuchrole\" does not exist"
    );
}

#[test]
fn set_role_default_none_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "SET ROLE DEFAULT");
    ddl(&mut e, "SET ROLE NONE");
}

#[test]
fn set_role_string_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "SET ROLE 'postgres'");
}

#[test]
fn set_session_role_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "SET SESSION ROLE postgres");
    ddl(&mut e, "CREATE ROLE alice LOGIN PASSWORD 'x'");
    ddl(&mut e, "SET LOCAL ROLE alice");
}
