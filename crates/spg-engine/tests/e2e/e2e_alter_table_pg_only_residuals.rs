//! v7.37.18 (18.18) — remaining PG-only ALTER TABLE sub-commands
//! accept-and-no-op for pg_dump round-trip.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn reset_storage_params_no_ops() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "ALTER TABLE t RESET (fillfactor)");
    ddl(
        &mut e,
        "ALTER TABLE t RESET (autovacuum_enabled, autovacuum_vacuum_threshold)",
    );
}

#[test]
fn alter_of_type_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "ALTER TABLE t OF some_composite_type");
    ddl(&mut e, "ALTER TABLE t NOT OF");
}

#[test]
fn force_row_level_security_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "ALTER TABLE t FORCE ROW LEVEL SECURITY");
    ddl(&mut e, "ALTER TABLE t NO FORCE ROW LEVEL SECURITY");
}

#[test]
fn enable_disable_row_level_security_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
    ddl(&mut e, "ALTER TABLE t DISABLE ROW LEVEL SECURITY");
}

#[test]
fn enable_disable_trigger_still_works() {
    // Regression — the RLS arm's guard must require the next token
    // to be ROW; otherwise the ENABLE/DISABLE TRIGGER path stops
    // matching.
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(
        &mut e,
        "CREATE OR REPLACE FUNCTION trg_fn() RETURNS trigger AS $$ BEGIN RETURN NEW; END $$ LANGUAGE plpgsql",
    );
    ddl(
        &mut e,
        "CREATE TRIGGER t_trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg_fn()",
    );
    ddl(&mut e, "ALTER TABLE t DISABLE TRIGGER t_trg");
    ddl(&mut e, "ALTER TABLE t ENABLE TRIGGER t_trg");
    ddl(&mut e, "ALTER TABLE t DISABLE TRIGGER ALL");
    ddl(&mut e, "ALTER TABLE t ENABLE TRIGGER ALL");
}
