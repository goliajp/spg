//! v7.37.20 (20.12) — PL/pgSQL PERFORM statement.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn perform_constant_expression_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "DO $$ BEGIN PERFORM 1; END $$;");
}

#[test]
fn perform_select_count_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (1), (2)");
    ddl(&mut e, "DO $$ BEGIN PERFORM count(*) FROM t; END $$;");
}

#[test]
fn perform_does_not_leak_rows_to_caller() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (42)");
    // The result of PERFORM must NOT propagate to the caller —
    // a DO block returning a row set would error from the
    // executor. The fact that this DO block runs cleanly is
    // the contract.
    ddl(&mut e, "DO $$ BEGIN PERFORM id FROM t; END $$;");
}
