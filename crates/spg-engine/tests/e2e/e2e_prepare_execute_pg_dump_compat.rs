//! v7.37.17 (17.6 siblings) — SQL-level PREPARE / EXECUTE.
//!
//! v7.39 (round 277) — these were `*_no_op` tests that asserted the
//! statements parse and do nothing. They now DO something, so the
//! no-op assertions were locking in the very behaviour that round
//! removed: `EXECUTE myplan(42)` against a session that never
//! prepared `myplan` reported success and returned no rows, where PG
//! raises `prepared statement "myplan" does not exist`. Reasserted
//! against live PG 18.4.

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
fn execute_of_an_unprepared_name_errors() {
    let mut e = Engine::new();
    for sql in ["EXECUTE myplan(42)", "EXECUTE myplan"] {
        let msg = format!("{:?}", e.execute(sql).unwrap_err());
        assert!(
            msg.contains(r#"prepared statement \"myplan\" does not exist"#),
            "{sql}: {msg}",
        );
    }
}

#[test]
fn a_prepared_plan_executes() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (42)");
    ddl(
        &mut e,
        "PREPARE myplan (int) AS SELECT id FROM t WHERE id = $1",
    );
    ddl(&mut e, "EXECUTE myplan(42)");
}
