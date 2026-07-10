//! v7.37.20 (20.11) — PL/pgSQL RETURN QUERY / RETURN QUERY EXECUTE
//! desugared to PERFORM-equivalent inside DO block context.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn return_query_select_runs_for_side_effects_in_do() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (1)");
    // RETURN QUERY <select> in a DO block runs the select and
    // discards. The DO block completes cleanly.
    ddl(&mut e, "DO $$ BEGIN RETURN QUERY SELECT id FROM t; END $$;");
}

#[test]
fn return_query_execute_runs_dynamic_sql_in_do() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (7)");
    ddl(
        &mut e,
        "DO $$ BEGIN RETURN QUERY EXECUTE 'SELECT id FROM t'; END $$;",
    );
}

#[test]
fn return_null_still_works() {
    let mut e = Engine::new();
    ddl(&mut e, "DO $$ BEGIN RETURN NULL; END $$;");
}
