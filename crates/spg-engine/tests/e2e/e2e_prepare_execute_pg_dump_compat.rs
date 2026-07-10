//! v7.37.17 (17.6 siblings) — SQL-level PREPARE / EXECUTE
//! parse-accept for driver / ORM code paths that emit them.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn prepare_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(
        &mut e,
        "PREPARE myplan (int) AS SELECT id FROM t WHERE id = $1",
    );
}

#[test]
fn execute_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "EXECUTE myplan(42)");
    ddl(&mut e, "EXECUTE myplan");
}
