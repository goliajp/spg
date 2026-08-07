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

/// v7.39 (round 308, V29) — restated, not deleted. The named form used
/// to be a no-op, so this passed against an engine that had never heard
/// of those constraints. PG refuses that outright (`constraint "…" does
/// not exist`), and the named form now means what it says — so the
/// dump-compat claim has to be made against a schema that HAS them.
#[test]
fn set_constraints_named_applies_to_the_named_constraints() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE customers (id int PRIMARY KEY)");
    ddl(
        &mut e,
        "CREATE TABLE orders (id int, cid int CONSTRAINT fk_orders_customer \
         REFERENCES customers(id) DEFERRABLE)",
    );
    ddl(&mut e, "BEGIN");
    ddl(&mut e, "SET CONSTRAINTS fk_orders_customer DEFERRED");
    ddl(&mut e, "ROLLBACK");
}

/// The other half of the same statement: a name nothing owns is PG's
/// error, not a silent success.
#[test]
fn set_constraints_named_unknown_is_an_error() {
    let mut e = Engine::new();
    ddl(&mut e, "BEGIN");
    let msg = format!(
        "{:?}",
        e.execute("SET CONSTRAINTS uq_orders_ref DEFERRED")
            .unwrap_err()
    );
    assert!(msg.contains("does not exist"), "got {msg}");
}
