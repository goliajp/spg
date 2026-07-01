//! v7.37.17 (17.6 siblings) — pg_dump-compat session-state
//! statements: DISCARD / DEALLOCATE / SECURITY LABEL.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn discard_all_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "DISCARD ALL");
    ddl(&mut e, "DISCARD PLANS");
    ddl(&mut e, "DISCARD SEQUENCES");
    ddl(&mut e, "DISCARD TEMPORARY");
    ddl(&mut e, "DISCARD TEMP");
}

#[test]
fn deallocate_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "DEALLOCATE myplan");
    ddl(&mut e, "DEALLOCATE PREPARE myplan");
    ddl(&mut e, "DEALLOCATE ALL");
}

#[test]
fn security_label_on_object_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "SECURITY LABEL ON TABLE t IS 'unclassified'");
    ddl(
        &mut e,
        "SECURITY LABEL FOR selinux ON TABLE t IS 'system_u:object_r:sepgsql_table_t:s0'",
    );
}
