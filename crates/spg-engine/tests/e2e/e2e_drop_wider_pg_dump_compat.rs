//! v7.37.17 (17.6 siblings) — pg_dump-compat wider DROP targets:
//! EXTENSION / TYPE / DOMAIN / AGGREGATE / OPERATOR / CAST /
//! COLLATION / LANGUAGE / CONVERSION / TEXT SEARCH / FOREIGN * /
//! SERVER / MATERIALIZED VIEW / EVENT TRIGGER / TABLESPACE / RULE /
//! POLICY / LARGE OBJECT / ROLE / ACCESS METHOD / STATISTICS /
//! PROCEDURE / ROUTINE.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn drop_extension_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "DROP EXTENSION IF EXISTS pg_trgm CASCADE");
    ddl(&mut e, "DROP EXTENSION plpgsql");
}

#[test]
fn drop_type_domain_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "DROP TYPE IF EXISTS mytype CASCADE");
    ddl(&mut e, "DROP DOMAIN IF EXISTS positive_int");
}

#[test]
fn drop_aggregate_operator_cast_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "DROP AGGREGATE myavg(int)");
    ddl(&mut e, "DROP OPERATOR + (int, int) CASCADE");
    ddl(&mut e, "DROP CAST (int AS text)");
}

#[test]
fn drop_language_collation_conversion_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "DROP LANGUAGE plpythonu CASCADE");
    ddl(&mut e, "DROP COLLATION IF EXISTS \"my_collation\"");
    ddl(&mut e, "DROP CONVERSION IF EXISTS ascii_to_utf8");
}

#[test]
fn drop_text_search_foreign_server_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "DROP TEXT SEARCH CONFIGURATION IF EXISTS english");
    ddl(&mut e, "DROP FOREIGN TABLE IF EXISTS foreign_t");
    ddl(&mut e, "DROP FOREIGN DATA WRAPPER pgfdw CASCADE");
    ddl(&mut e, "DROP SERVER myserver");
}

#[test]
fn drop_matview_event_tablespace_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "DROP MATERIALIZED VIEW IF EXISTS mv CASCADE");
    ddl(&mut e, "DROP EVENT TRIGGER my_trg");
    ddl(&mut e, "DROP TABLESPACE IF EXISTS mytbs");
}

#[test]
fn drop_rule_policy_large_object_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "DROP RULE IF EXISTS notify_me ON t");
    ddl(&mut e, "DROP POLICY IF EXISTS my_policy ON t");
    ddl(&mut e, "DROP LARGE OBJECT 12345");
}

#[test]
fn drop_role_access_procedure_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "DROP ROLE IF EXISTS alice");
    ddl(&mut e, "DROP ACCESS METHOD IF EXISTS heap2");
    ddl(&mut e, "DROP PROCEDURE IF EXISTS myproc(int)");
    ddl(&mut e, "DROP ROUTINE IF EXISTS myrout(int)");
}
