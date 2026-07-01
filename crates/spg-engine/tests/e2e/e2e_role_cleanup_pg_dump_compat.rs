//! v7.37.17 (17.6 siblings) — pg_dumpall-compat role cleanup
//! statements: REASSIGN OWNED / DROP OWNED.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn reassign_owned_by_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "REASSIGN OWNED BY olduser TO newuser");
    ddl(&mut e, "REASSIGN OWNED BY a, b, c TO admin");
}

#[test]
fn drop_owned_by_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "DROP OWNED BY olduser");
    ddl(&mut e, "DROP OWNED BY olduser CASCADE");
    ddl(&mut e, "DROP OWNED BY a, b, c RESTRICT");
}
