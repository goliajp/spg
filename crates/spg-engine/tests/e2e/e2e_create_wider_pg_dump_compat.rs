//! v7.37.17 (17.6 siblings) — pg_dump-compat wider CREATE targets:
//! TEXT SEARCH / SERVER / TABLESPACE / ACCESS METHOD / LARGE OBJECT.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn create_text_search_no_op() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "CREATE TEXT SEARCH CONFIGURATION mycfg (COPY = english)",
    );
    ddl(
        &mut e,
        "CREATE TEXT SEARCH DICTIONARY mydict (TEMPLATE = simple)",
    );
    ddl(
        &mut e,
        "CREATE TEXT SEARCH PARSER myparser (START = 'x', GETTOKEN = 'y', END = 'z', LEXTYPES = 'w')",
    );
    ddl(&mut e, "CREATE TEXT SEARCH TEMPLATE mytmpl (LEXIZE = 'x')");
}

#[test]
fn create_server_tablespace_no_op() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "CREATE SERVER myserver FOREIGN DATA WRAPPER pgfdw OPTIONS (host 'localhost', dbname 'mydb')",
    );
    ddl(&mut e, "CREATE TABLESPACE mytbs LOCATION '/data/mytbs'");
}

#[test]
fn create_access_method_large_object_no_op() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "CREATE ACCESS METHOD heap2 TYPE TABLE HANDLER heap2_handler",
    );
    // No literal CREATE LARGE OBJECT in the PG grammar but lo_create()
    // isn't a CREATE — leave large-object test to DROP LARGE OBJECT
    // shape which is real.
}
