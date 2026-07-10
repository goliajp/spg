//! v7.37.17 (17.6 sibling) — SET CONSTRAINTS pg_dump-compat.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn set_constraints_all_deferred_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "SET CONSTRAINTS ALL DEFERRED");
}

#[test]
fn set_constraints_all_immediate_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "SET CONSTRAINTS ALL IMMEDIATE");
}

#[test]
fn set_constraints_named_no_op() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "SET CONSTRAINTS fk_orders_customer, uq_orders_ref DEFERRED",
    );
}
