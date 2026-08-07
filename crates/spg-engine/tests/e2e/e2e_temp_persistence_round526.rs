//! v7.39 (round 526) — a temporary relation says so in the catalog.
//!
//! The v7.37 audit listed "temp sequence / temp view return ok but
//! create PERMANENT objects" as silent-wrong. Re-measuring narrowed it:
//! the objects ARE session-scoped — a second connection cannot see them
//! and `nextval` on one answers "does not exist" — but every one of them
//! reported `relpersistence = 'p'`, including the temp TABLE that round
//! 436 implemented. PG answers `'t'`.
//!
//! It is the column a tool reads to list temp objects, and the one a
//! migration tool reads to decide what to dump, so reporting permanent
//! is how a session-scoped object ends up in a schema diff.
//!
//! Measured alongside it: `relnamespace::regnamespace` — the way a
//! catalog join names a schema — failed with "unsupported cast target",
//! while `'public'::regnamespace` worked. Round 513 added the NAME
//! direction of the reg types and not the NUMERIC one, so the half a
//! catalog actually uses was the half missing.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    Engine::new()
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// All three relation kinds, temporary and permanent.
#[test]
fn round526_relpersistence_says_temporary() {
    let mut e = engine();
    e.execute("CREATE TEMP SEQUENCE q1").unwrap();
    e.execute("CREATE TEMP VIEW q2 AS SELECT 1 AS a").unwrap();
    e.execute("CREATE TEMP TABLE q3 (a INT)").unwrap();
    e.execute("CREATE TABLE q4 (a INT)").unwrap();
    e.execute("CREATE SEQUENCE q5").unwrap();
    e.execute("CREATE VIEW q6 AS SELECT 1 AS a").unwrap();
    for (name, expect) in [
        ("q1", "S|t"),
        ("q2", "v|t"),
        ("q3", "r|t"),
        ("q4", "r|p"),
        ("q5", "S|p"),
        ("q6", "v|p"),
    ] {
        assert_eq!(
            text(
                &mut e,
                &format!("SELECT relkind, relpersistence FROM pg_class WHERE relname = '{name}'")
            ),
            expect,
            "pg_class row for {name}"
        );
    }
}

/// The filter a tool actually writes.
#[test]
fn round526_temp_objects_are_findable_by_persistence() {
    let mut e = engine();
    e.execute("CREATE TEMP TABLE t1 (a INT)").unwrap();
    e.execute("CREATE TABLE p1 (a INT)").unwrap();
    assert_eq!(
        text(
            &mut e,
            "SELECT relname FROM pg_class WHERE relpersistence = 't' ORDER BY relname"
        ),
        "t1"
    );
}

/// An oid cast to a reg type — the direction a catalog join uses.
#[test]
fn round526_numeric_reg_casts_resolve() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT (2200::regnamespace)::text, (11::regnamespace)::text"
        ),
        "public|pg_catalog"
    );
    // An oid that names nothing prints as the bare number, as regclass does.
    assert_eq!(text(&mut e, "SELECT (99999::regnamespace)::text"), "99999");
    // The name direction still works.
    assert_eq!(
        text(&mut e, "SELECT ('public'::regnamespace)::text"),
        "public"
    );
    // And a role oid.
    assert_eq!(text(&mut e, "SELECT (10::regrole)::text"), "postgres");
}

/// The join that could not be written before.
#[test]
fn round526_regnamespace_names_a_relations_schema() {
    let mut e = engine();
    e.execute("CREATE TABLE rr (a INT)").unwrap();
    assert_eq!(
        text(
            &mut e,
            "SELECT relname, (relnamespace::regnamespace)::text \
             FROM pg_class WHERE relname = 'rr'"
        ),
        "rr|public"
    );
}
