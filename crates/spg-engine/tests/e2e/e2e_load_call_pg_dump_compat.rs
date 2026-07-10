//! v7.37.17 (17.6 siblings) — LOAD / CALL pg_dump-compat.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn load_shared_library_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "LOAD 'my_extension'");
    ddl(&mut e, "LOAD 'pg_stat_statements'");
}

#[test]
fn call_procedure_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "CALL my_proc()");
    ddl(&mut e, "CALL update_stats(1, 'x')");
    ddl(&mut e, "CALL public.rebuild_analytics()");
}
