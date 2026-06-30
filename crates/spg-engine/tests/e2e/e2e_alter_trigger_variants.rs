//! v7.37.18 (18.12) — ENABLE/DISABLE TRIGGER variants beyond
//! ALL / <name>. PG dumps emit:
//!     ENABLE TRIGGER USER         — all user-defined triggers
//!     ENABLE TRIGGER REPLICA      — replica-only (session_replication_role)
//!     ENABLE TRIGGER ALWAYS       — always run regardless of role
//!     ENABLE ALWAYS TRIGGER <name>
//!     ENABLE REPLICA TRIGGER <name>
//! SPG has no replication role, so these all reduce to the
//! existing ALL or Named selector; this commit makes the parser
//! accept the forms cleanly.

use spg_engine::Engine;

fn engine_with_trigger() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute(
        "CREATE FUNCTION noop_fn() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .unwrap();
    e.execute(
        "CREATE TRIGGER ix_trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION noop_fn()",
    )
    .unwrap();
    e
}

#[test]
fn enable_disable_trigger_user_accepted() {
    let mut e = engine_with_trigger();
    e.execute("ALTER TABLE t ENABLE TRIGGER USER").unwrap();
    e.execute("ALTER TABLE t DISABLE TRIGGER USER").unwrap();
}

#[test]
fn enable_disable_trigger_replica_accepted() {
    let mut e = engine_with_trigger();
    e.execute("ALTER TABLE t ENABLE TRIGGER REPLICA").unwrap();
    e.execute("ALTER TABLE t DISABLE TRIGGER REPLICA").unwrap();
}

#[test]
fn enable_disable_trigger_always_accepted() {
    let mut e = engine_with_trigger();
    e.execute("ALTER TABLE t ENABLE TRIGGER ALWAYS").unwrap();
    e.execute("ALTER TABLE t DISABLE TRIGGER ALWAYS").unwrap();
}

#[test]
fn enable_replica_trigger_named_accepted() {
    let mut e = engine_with_trigger();
    e.execute("ALTER TABLE t ENABLE REPLICA TRIGGER ix_trg")
        .unwrap();
    e.execute("ALTER TABLE t ENABLE ALWAYS TRIGGER ix_trg")
        .unwrap();
    e.execute("ALTER TABLE t DISABLE TRIGGER ix_trg").unwrap();
}

#[test]
fn enable_disable_trigger_all_still_works() {
    // Regression guard for the existing ALL / <name> path.
    let mut e = engine_with_trigger();
    e.execute("ALTER TABLE t DISABLE TRIGGER ALL").unwrap();
    e.execute("ALTER TABLE t ENABLE TRIGGER ALL").unwrap();
    e.execute("ALTER TABLE t DISABLE TRIGGER ix_trg").unwrap();
    e.execute("ALTER TABLE t ENABLE TRIGGER ix_trg").unwrap();
}
