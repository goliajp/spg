//! v7.37.17 (17.6 sibling) — SET ROLE pg_dump-compat.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn set_role_named_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "SET ROLE postgres");
    ddl(&mut e, "SET ROLE alice");
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
    ddl(&mut e, "SET LOCAL ROLE alice");
}
