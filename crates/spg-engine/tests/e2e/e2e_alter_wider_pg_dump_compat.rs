//! v7.37.17 (17.6 siblings) — pg_dump-compat wider ALTER targets.
//! ALTER SYSTEM / USER / GROUP / TABLESPACE / COLLATION / AGGREGATE
//! / LANGUAGE / OPERATOR / CONVERSION / STATISTICS / SERVER /
//! FOREIGN TABLE / TEXT SEARCH / EVENT TRIGGER / LARGE OBJECT.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn alter_system_set_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "ALTER SYSTEM SET work_mem = '64MB'");
    ddl(&mut e, "ALTER SYSTEM RESET ALL");
}

#[test]
fn alter_user_no_op() {
    // ALTER GROUP hits Token::Group (reserved for GROUP BY) so
    // routes through the token-level branch, not the ident branch;
    // pg_dumpall doesn't emit ALTER GROUP for modern PG releases
    // (uses ALTER ROLE instead) so leaving it un-added is safe.
    let mut e = Engine::new();
    ddl(&mut e, "ALTER USER alice WITH PASSWORD 'x'");
}

#[test]
fn alter_tablespace_collation_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "ALTER TABLESPACE pg_default OWNER TO postgres");
    ddl(&mut e, "ALTER COLLATION \"en_US\" REFRESH VERSION");
}

#[test]
fn alter_aggregate_language_operator_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "ALTER AGGREGATE myavg(int) RENAME TO my_avg");
    ddl(&mut e, "ALTER LANGUAGE plpgsql OWNER TO postgres");
    ddl(&mut e, "ALTER OPERATOR + (int, int) SET SCHEMA public");
}

#[test]
fn alter_server_foreign_no_op() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "ALTER SERVER myserver OPTIONS (SET host 'localhost')",
    );
    ddl(&mut e, "ALTER FOREIGN TABLE foreign_t RENAME TO ft");
    ddl(
        &mut e,
        "ALTER FOREIGN DATA WRAPPER pgfdw OPTIONS (SET debug 'on')",
    );
}

#[test]
fn alter_text_search_event_large_no_op() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "ALTER TEXT SEARCH CONFIGURATION english OWNER TO postgres",
    );
    ddl(&mut e, "ALTER EVENT TRIGGER my_trg DISABLE");
    ddl(&mut e, "ALTER LARGE OBJECT 12345 OWNER TO postgres");
}
