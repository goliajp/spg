//! 8.0.2 — pg_dump's `typrelkind` CASE, which a correlated scalar
//! subquery's type claim broke.
//!
//! pg_dump asks this of every server it dumps:
//!
//! ```text
//!   CASE WHEN typrelid = 0 THEN ' '::"char"
//!        ELSE (SELECT relkind FROM pg_class WHERE oid = typrelid)
//!   END AS typrelkind
//!   FROM pg_type
//! ```
//!
//! 8.0.1 answered it. 8.0.2's §3.9 fix taught the CORRELATED scalar
//! subquery path to carry its column's declared type into the literal
//! it substitutes, and this engine's catalog declares `relkind` `text`
//! where PostgreSQL declares it `"char"` — so the branch began claiming
//! `text`, and the CASE refused:
//!
//! ```text
//!   ERROR:  CASE types text and "char" cannot be matched
//! ```
//!
//! PostgreSQL raises that same sentence for a genuine text/"char" mix,
//! so the check is right and the claim was wrong. `pg_dump` exited 1
//! against SPG, which is `pgdump-roundtrip` — a step that had not run
//! for five tier runs because something earlier failed first, so the
//! regression sat unseen from the commit that caused it.
//!
//! Every expectation here is PostgreSQL 18.6's own answer for the same
//! shape: a composite type's row reads `c`, a scalar type's row reads a
//! single space.
//!
//! What is NOT fixed, and is filed: 34 catalog columns are declared
//! `text` here and `"char"` in PostgreSQL. See
//! `substitute::correlated_declared_type` for the three measured gaps
//! that closing it needs.

use spg_engine::{Engine, QueryResult};

const TYPRELKIND: &str = "CASE WHEN typrelid = 0 THEN ' '::\"char\" \
     ELSE (SELECT relkind FROM pg_class WHERE oid = typrelid) END";

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE plain (id int)",
        "CREATE TYPE addr2 AS (street text, zip int)",
    ] {
        e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"));
    }
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    rows.iter()
        .map(|r| {
            r.values
                .iter()
                .map(spg_engine::eval::value_to_text)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[test]
fn pg_dumps_typrelkind_case_answers_at_all() {
    let mut e = seeded();
    // The whole catalog, which is what pg_dump asks for. Before the fix
    // this raised rather than returning anything.
    let sql = alloc_fmt(&format!("SELECT {TYPRELKIND} FROM pg_type"));
    let QueryResult::Rows { rows, .. } = e
        .execute(&sql)
        .unwrap_or_else(|x| panic!("pg_dump's own CASE must answer: {x:?}"))
    else {
        panic!("expected Rows")
    };
    assert!(
        rows.len() > 10,
        "pg_type should carry the built-in types, got {} row(s)",
        rows.len()
    );
}

#[test]
fn a_composite_types_row_reads_c_as_postgresql_answers() {
    let mut e = seeded();
    assert_eq!(
        one(
            &mut e,
            &format!("SELECT typname, {TYPRELKIND} FROM pg_type WHERE typname = 'addr2'")
        ),
        "addr2|c"
    );
}

#[test]
fn a_scalar_types_row_reads_a_space_as_postgresql_answers() {
    let mut e = seeded();
    assert_eq!(
        one(
            &mut e,
            &format!("SELECT typname, {TYPRELKIND} FROM pg_type WHERE typname = 'int4'")
        ),
        "int4| "
    );
}

/// The reason the branch is not typed, kept beside the thing it
/// protects: §3.9's own shape must still carry its zone. If this breaks,
/// the fix reached past the claim and into the value.
#[test]
fn the_zone_a_correlated_subquery_carries_is_untouched() {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE iss (id int)",
        "INSERT INTO iss VALUES (1),(2),(3)",
        "CREATE TABLE ev (issue_id int, occurred_at timestamptz)",
        "INSERT INTO ev VALUES (1,'2026-01-01'),(2,'2026-02-01'),(2,'2026-03-01')",
    ] {
        e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"));
    }
    assert_eq!(
        one(
            &mut e,
            "SELECT i.id, (SELECT max(e.occurred_at) FROM ev e WHERE e.issue_id = i.id)::text \
             FROM iss i ORDER BY i.id"
        ),
        "1|2026-01-01 00:00:00+00,2|2026-03-01 00:00:00+00,3|NULL"
    );
}

fn alloc_fmt(s: &str) -> String {
    String::from(s)
}
