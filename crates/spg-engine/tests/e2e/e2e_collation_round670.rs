//! Round 670 — what SPG's C collation is, and what it is not.
//!
//! SPG orders text by bytes. That is the C collation, it is what
//! `pg_database.datcollate` advertises, and it is self-consistent. This file
//! pins that self-consistency.
//!
//! It deliberately does NOT pin the ORDER BY results themselves. Measured
//! against a PG18 running `en_US.utf8`, all nine ordinary shapes differ —
//! ORDER BY, min/max, `<`, DISTINCT, GROUP BY ordering, ROW_NUMBER, index
//! scan, and even ORDER BY upper(name) — and one of them differs in a way
//! that is not about order at all: `WHERE name BETWEEN 'B' AND 'c'` returns
//! four rows here and one there. Pinning those outputs would be pinning the
//! gap in place, which is how `SELECT age(12345) = 0` survived from round
//! 627 to round 668.
//!
//! The gap is F36 in the ledger. What makes it urgent is not the ordering:
//! it is that a customer's pg_dump declaring `en_US.utf8` restores CLEAN
//! here — CREATE DATABASE, CREATE TABLE ... COLLATE, ALTER ... COLLATE and
//! CREATE COLLATION are all accepted — and then every query silently uses
//! byte order. The declaration is taken and ignored.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

/// SPG says C and means C. These three have to agree with each other, or
/// the database is lying about itself in one of the places a client looks.
#[test]
fn round670_spg_advertises_c_consistently() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT datcollate FROM pg_database LIMIT 1"),
        "C"
    );
    assert_eq!(one(&mut e, "SELECT datctype FROM pg_database LIMIT 1"), "C");
    assert_eq!(one(&mut e, "SELECT current_setting('lc_monetary')"), "C");
    // And the catalog offers only the collations it can actually perform.
    assert_eq!(
        one(
            &mut e,
            "SELECT string_agg(collname, ',' ORDER BY collname) FROM pg_collation"
        ),
        "C,POSIX,default"
    );
}

/// Byte order, stated as a property rather than as a list of rows: an
/// uppercase letter sorts before a lowercase one, which is the whole of the
/// difference from a locale collation in the ASCII range.
#[test]
fn round670_text_orders_by_bytes() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 'B' < 'a'"), "true");
    assert_eq!(one(&mut e, "SELECT 'Z' < 'a'"), "true");
    assert_eq!(one(&mut e, "SELECT '_' < 'a'"), "true");
    // A C collation names itself in a comparison and changes nothing.
    assert_eq!(one(&mut e, "SELECT 'B' < 'a' COLLATE \"C\""), "true");
}

/// Where SPG cannot perform a collation it says so rather than pretending.
/// These two paths already refuse; the ledger's F36 records the DDL paths
/// that do not, which is the part that lets a dump restore clean and then
/// answer differently.
#[test]
fn round670_a_locale_collation_is_refused_where_it_is_asked_for() {
    let mut e = Engine::new();
    let msg = err(&mut e, "SELECT 'a' < 'B' COLLATE \"en_US\"");
    assert!(msg.contains("orders text by bytes"), "{msg}");
    assert!(
        msg.contains("not supported yet"),
        "the refusal should say it is a gap, not a rule: {msg}"
    );

    e.execute("CREATE TABLE ct(name TEXT)").unwrap();
    let msg = err(&mut e, "CREATE INDEX ON ct (name COLLATE \"en_US\")");
    assert!(msg.contains("orders text by bytes"), "{msg}");
}
