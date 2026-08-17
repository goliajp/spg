//! r1052 — a synthesised catalog keeps its WRITTEN name as a binding
//! qualifier, and pg_dump's cast spelling parses.
//!
//! `FROM pg_cast WHERE pg_cast.oid > 16383` answered "missing
//! FROM-clause entry for table pg_cast": the parser rewrites the
//! catalog reference to `__spg_pg_cast`, and the qualifier a user (or
//! pg_dump) writes stayed the written name with nothing to bind to.
//! Inside `EXISTS` the same shape surfaced as the engine's own
//! "subquery reached row eval — engine resolver bug". The rewrite now
//! keeps the written name as the relation's alias — PG semantics: the
//! visible name of `pg_catalog.pg_cast` IS `pg_cast`.
//!
//! Found by running real pg_dump against SPGS (the drop-in gold
//! standard): its FIRST catalog query died on this. The same session
//! also hit `amhandler::pg_catalog.regproc` — pg_dump
//! schema-qualifies every cast target — pinned below.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The four spellings that must agree: unqualified, alias-qualified,
/// name-qualified, schema-qualified-then-name-qualified.
#[test]
fn r1052_catalog_written_name_binds_as_qualifier() {
    let mut e = Engine::new();
    let n0 = one(&mut e, "SELECT count(*) FROM pg_cast WHERE oid <> 0");
    for sql in [
        "SELECT count(*) FROM pg_cast c WHERE c.oid <> 0",
        "SELECT count(*) FROM pg_cast WHERE pg_cast.oid <> 0",
        "SELECT count(*) FROM pg_catalog.pg_cast WHERE pg_cast.oid <> 0",
    ] {
        assert_eq!(one(&mut e, sql), n0, "{sql}");
    }
    assert_ne!(n0, "0", "pg_cast must have rows for the pin to bind");
}

/// The EXISTS shapes that reached row eval.
#[test]
fn r1052_exists_over_a_catalog_with_name_qualifier() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT EXISTS (SELECT 1 FROM pg_cast WHERE pg_cast.oid <> 0)"
        ),
        "true"
    );
    // The pg_dump pg_proc shape in miniature: outer correlation plus a
    // name-qualified inner catalog.
    let n = one(
        &mut e,
        "SELECT count(*) FROM pg_proc p WHERE EXISTS \
         (SELECT 1 FROM pg_cast WHERE pg_cast.oid <> 0 AND p.oid = pg_cast.castfunc)",
    );
    let n2 = one(
        &mut e,
        "SELECT count(DISTINCT castfunc) FROM pg_cast WHERE castfunc IN (SELECT oid FROM pg_proc)",
    );
    assert_eq!(n, n2, "correlated EXISTS agrees with the IN rewrite");
}

/// An explicit alias still wins over the written name.
#[test]
fn r1052_an_explicit_alias_replaces_the_written_name() {
    let mut e = Engine::new();
    let err = e
        .execute("SELECT count(*) FROM pg_cast c WHERE pg_cast.oid <> 0")
        .expect_err("with an alias, the written name must stop binding (PG semantics)");
    let msg = format!("{err}");
    assert!(msg.contains("pg_cast"), "{msg}");
}

/// `::pg_catalog.<type>` — pg_dump's spelling for every cast.
#[test]
fn r1052_schema_qualified_cast_target_parses() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 42::pg_catalog.int8"), "42");
    assert_eq!(one(&mut e, "SELECT 'x'::pg_catalog.text"), "x");
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_am WHERE amhandler::pg_catalog.regproc IS NOT NULL"
        ),
        one(
            &mut e,
            "SELECT count(*) FROM pg_am WHERE amhandler IS NOT NULL"
        ),
    );
}
