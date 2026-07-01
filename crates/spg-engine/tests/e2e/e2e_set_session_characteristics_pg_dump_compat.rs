//! v7.37.17 (17.6 sibling) — SET SESSION CHARACTERISTICS pg_dump-compat.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn set_session_characteristics_isolation_no_op() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL READ COMMITTED",
    );
    ddl(
        &mut e,
        "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL SERIALIZABLE",
    );
    ddl(
        &mut e,
        "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ",
    );
}

#[test]
fn set_session_characteristics_read_write_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE");
    ddl(&mut e, "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY");
}
