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
    // v7.39 (round 707) — DROP AGGREGATE is real now: an unknown name is
    // refused as PG refuses it, and a dump only drops aggregates it will
    // recreate — with IF EXISTS when the target may be absent, which is
    // the spelling pg_dump --clean emits. The old bare-name assertion was
    // pinning the swallow (the F31 shape).
    ddl(&mut e, "DROP AGGREGATE IF EXISTS myavg(int)");
    let err = e
        .execute("DROP AGGREGATE myavg(int)")
        .expect_err("PG18 refuses an unknown aggregate");
    assert!(format!("{err}").contains("aggregate myavg(integer) does not exist"));
    ddl(&mut e, "DROP OPERATOR + (int, int) CASCADE");
    ddl(&mut e, "DROP CAST (int AS text)");
}

#[test]
fn drop_language_collation_conversion_no_op() {
    let mut e = Engine::new();
    // v7.39 (round 708) — DROP LANGUAGE / DROP CONVERSION answer as PG
    // does: unknown names do not exist, shipped languages are required,
    // IF EXISTS stays quiet. The old bare-name assertions pinned the
    // swallow — F31.
    let err = e
        .execute("DROP LANGUAGE plpythonu CASCADE")
        .expect_err("PG18: language \"plpythonu\" does not exist");
    assert!(format!("{err}").contains("language \"plpythonu\" does not exist"));
    let err = e
        .execute("DROP LANGUAGE plpgsql")
        .expect_err("PG18 refuses to drop a required language");
    assert!(
        format!("{err}")
            .contains("cannot drop language plpgsql because extension plpgsql requires it")
    );
    ddl(&mut e, "DROP LANGUAGE IF EXISTS plpythonu");
    ddl(&mut e, "DROP COLLATION IF EXISTS \"my_collation\"");
    ddl(&mut e, "DROP CONVERSION IF EXISTS ascii_to_utf8");
    let err = e
        .execute("DROP CONVERSION ascii_to_utf8")
        .expect_err("PG-shaped not-found for a conversion SPG does not ship");
    assert!(format!("{err}").contains("conversion \"ascii_to_utf8\" does not exist"));
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
    // v7.39 (round 709) — the name check is real; IF EXISTS is the quiet
    // spelling. The old bare assertion pinned the swallow — F31.
    let err = e
        .execute("DROP EVENT TRIGGER my_trg")
        .expect_err("PG18 refuses an unknown event trigger");
    assert!(format!("{err}").contains("event trigger \"my_trg\" does not exist"));
    ddl(&mut e, "DROP EVENT TRIGGER IF EXISTS my_trg");
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
