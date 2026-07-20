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
fn call_reports_the_missing_procedure() {
    // v7.39 (round 278) — was `call_procedure_no_op`, which asserted
    // that a stored-procedure invocation succeeds and does nothing.
    // An application calling a procedure and being told it worked is
    // the worst answer available; PG reports
    // `procedure my_proc() does not exist`, and SPG has no procedure
    // catalog, so every CALL names one that does not.
    let mut e = Engine::new();
    for sql in [
        "CALL my_proc()",
        "CALL update_stats(1, 'x')",
        "CALL public.rebuild_analytics()",
    ] {
        let msg = format!("{:?}", e.execute(sql).unwrap_err());
        assert!(
            msg.contains("does not exist") && msg.contains("procedure"),
            "{sql}: {msg}",
        );
    }
}
