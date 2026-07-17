//! v7.39 (read01 round 60) — privileges on the NON-table objects: sequences,
//! the schema, the database.
//!
//! The privilege trilogy (r57 tables, r58 roles, r59 columns) left these three
//! as accepted no-ops, and `has_schema_privilege` / `has_sequence_privilege` /
//! `has_database_privilege` still answered an unconditional `true`. Same lie,
//! last three places.
//!
//! The trap: PG's default for these is NOT "nobody holds anything". PUBLIC holds
//! USAGE on the `public` schema and CONNECT + TEMPORARY on the database out of
//! the box — but NOT CREATE on either (PG 15 revoked the schema one). A model
//! that started them empty would deny every role's first SELECT; one that
//! started them full would let any role create tables. Byte-locked against
//! live PG18.4.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    ok(&mut e, "CREATE ROLE eve LOGIN PASSWORD 'x'");
    ok(&mut e, "CREATE SEQUENCE sq");
    e
}

#[test]
fn the_public_schema_grants_usage_but_not_create() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT has_schema_privilege('eve','public','USAGE')"
        ),
        "true"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT has_schema_privilege('eve','public','CREATE')"
        ),
        "false"
    );
    // nspacl is never NULL — PG prints the default it ships with.
    assert_eq!(
        r1(
            &mut e,
            "SELECT nspacl FROM pg_namespace WHERE nspname='public'"
        ),
        "{pg_database_owner=UC/pg_database_owner,=U/pg_database_owner}"
    );
}

#[test]
fn creating_a_table_needs_create_on_the_schema() {
    let mut e = seeded();
    ok(&mut e, "SET ROLE eve");
    assert_eq!(
        err(&mut e, "CREATE TABLE eve_t (a int)"),
        "unsupported: permission denied for schema public"
    );
    ok(&mut e, "RESET ROLE");
    ok(&mut e, "GRANT CREATE ON SCHEMA public TO eve");
    // The grant must not silently take PUBLIC's implicit USAGE away.
    assert_eq!(
        r1(
            &mut e,
            "SELECT has_schema_privilege('eve','public','USAGE')"
        ),
        "true"
    );
    ok(&mut e, "SET ROLE eve");
    ok(&mut e, "CREATE TABLE eve_t (a int)");
    assert_eq!(r1(&mut e, "SELECT count(*) FROM eve_t"), "0");
}

#[test]
fn a_sequence_has_its_own_three_privileges() {
    let mut e = seeded();
    assert_eq!(
        r1(&mut e, "SELECT has_sequence_privilege('eve','sq','USAGE')"),
        "false"
    );
    ok(&mut e, "GRANT USAGE ON SEQUENCE sq TO eve");
    // USAGE alone — not SELECT, not UPDATE.
    assert_eq!(
        r1(&mut e, "SELECT has_sequence_privilege('eve','sq','USAGE')"),
        "true"
    );
    assert_eq!(
        r1(&mut e, "SELECT has_sequence_privilege('eve','sq','UPDATE')"),
        "false"
    );
    // The owner's default is `rwU` — a sequence has no INSERT / DELETE / …
    assert_eq!(
        r1(&mut e, "SELECT relacl FROM pg_class WHERE relname='sq'"),
        "{admin=rwU/admin,eve=U/admin}"
    );
}

#[test]
fn nextval_needs_usage_and_setval_needs_update() {
    let mut e = seeded();
    ok(&mut e, "GRANT USAGE ON SEQUENCE sq TO eve");
    ok(&mut e, "SET ROLE eve");
    // nextval: USAGE or UPDATE. currval: USAGE or SELECT.
    assert_eq!(r1(&mut e, "SELECT nextval('sq')"), "1");
    assert_eq!(r1(&mut e, "SELECT currval('sq')"), "1");
    // setval takes UPDATE, which USAGE does not carry.
    assert_eq!(
        err(&mut e, "SELECT setval('sq', 10)"),
        "unsupported: permission denied for sequence sq"
    );
    // And the whole thing goes away when the grant does.
    ok(&mut e, "RESET ROLE");
    ok(&mut e, "REVOKE USAGE ON SEQUENCE sq FROM eve");
    ok(&mut e, "SET ROLE eve");
    assert_eq!(
        err(&mut e, "SELECT nextval('sq')"),
        "unsupported: permission denied for sequence sq"
    );
}

#[test]
fn the_database_grants_connect_but_not_create() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT has_database_privilege('eve','app','CONNECT')"
        ),
        "true"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT has_database_privilege('eve','app','CREATE')"
        ),
        "false"
    );
}

#[test]
fn a_superuser_session_is_untouched() {
    // The default login never assumes a role, so none of this applies to it.
    let mut e = seeded();
    ok(&mut e, "CREATE TABLE t (a int)");
    assert_eq!(r1(&mut e, "SELECT nextval('sq')"), "1");
    ok(&mut e, "SELECT setval('sq', 10)");
}
