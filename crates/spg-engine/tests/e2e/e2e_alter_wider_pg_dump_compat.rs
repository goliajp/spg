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

/// v7.39 (round 695) — `ALTER SYSTEM` now validates the parameter NAME.
///
/// The test above this one is called `alter_system_set_no_op` and sets
/// `work_mem` — a name that exists — so it could never have caught a name
/// that does not. PG18 answers `unrecognized configuration parameter`;
/// SPG accepted anything, because the whole statement was swallowed with
/// the pg_dump no-op tail.
///
/// Nothing is APPLIED either way: SPG has no postgresql.auto.conf. What
/// changed is that it says so about a name it does not know, reusing the
/// session's own GUC check so `SET` and `ALTER SYSTEM` cannot drift apart.
#[test]
fn round695_alter_system_rejects_an_unknown_parameter() {
    let mut e = Engine::new();
    let err = e
        .execute("ALTER SYSTEM SET nosuch_guc_695 = 1")
        .expect_err("PG18 refuses this");
    assert!(
        format!("{err}").contains("unrecognized configuration parameter"),
        "{err}"
    );
    // The forms PG accepts still pass, including a dotted customised
    // option (PG treats `myapp.thing` as one, and extensions rely on it).
    ddl(&mut e, "ALTER SYSTEM SET work_mem = '64MB'");
    ddl(&mut e, "ALTER SYSTEM RESET work_mem");
    ddl(&mut e, "ALTER SYSTEM RESET ALL");
    ddl(&mut e, "ALTER SYSTEM SET myapp.thing = 1");
}
