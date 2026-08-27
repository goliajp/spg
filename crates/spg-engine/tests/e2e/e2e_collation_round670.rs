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
    // And the catalogue offers the collations it can actually perform,
    // which as of v7.38.18 is PostgreSQL 18.4's whole list rather than
    // three names.
    //
    // This asserted `C,POSIX,default` when three rows were all there
    // were, and the sentence above it — "the database is lying about
    // itself in one of the places a client looks" — is exactly what the
    // three rows had become: a column could be declared `COLLATE
    // "en_US.utf8"`, `information_schema.columns` reported the name,
    // and this catalogue said no such collation existed.
    assert_eq!(one(&mut e, "SELECT count(*) FROM pg_collation"), "880");
    for name in [
        "C",
        "POSIX",
        "default",
        "en_US.utf8",
        "ucs_basic",
        "pg_c_utf8",
    ] {
        assert_eq!(
            one(&mut e, &alloc_q(name),),
            "1",
            "{name} must be in the catalogue"
        );
    }
    // A name PostgreSQL does not have is not in it either.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_collation WHERE collname = 'zz_ZZ'"
        ),
        "0"
    );
}

fn alloc_q(name: &str) -> String {
    format!("SELECT count(*) FROM pg_collation WHERE collname = '{name}'")
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

/// Where SPG cannot carry a collation it says where it CAN, rather than
/// telling the reader it orders by bytes.
///
/// v7.38.18 — this test used to require the words "orders text by bytes"
/// and "not supported yet", and the message said both. By the time it
/// was read, both were false: this build performs locale collations,
/// and a column declared `COLLATE "en_US.utf8"` orders `apple, client,
/// DateStyle, Zebra` — PG 18.4's answer, with `<`, `min()` and
/// `information_schema.columns` all agreeing. What SPG cannot do is
/// carry a collation on an arbitrary expression, because there is no
/// `Expr::Collate` to carry it.
///
/// The intent behind the old assertion survives and is what the second
/// half checks: a refusal must read as a GAP with somewhere else to go,
/// not as a rule about how SPG sorts. The ledger's F36 records the DDL
/// paths that accept what this one refuses.
#[test]
fn round670_a_locale_collation_is_carried_where_it_is_written() {
    // v7.39.2 — this asserted the refusal and the wording of it. The
    // refusal is gone: `Expr::Collate` carries the clause, and
    // PostgreSQL 18.6 answers `t` here — `en_US` is in its
    // `pg_collation`, measured, and so `'a' < 'B'` under it is true
    // where byte order says false.
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT 'a' < 'B' COLLATE \"en_US\""),
        "true",
        "PG 18.6 answers t for this"
    );
    assert_eq!(
        one(&mut e, "SELECT 'a' < 'B' COLLATE \"C\""),
        "false",
        "and byte order answers f, which absorbing the clause could not say"
    );

    // The INDEX key is a different position and is unchanged: its
    // collation rides a side channel the key itself owns, and a locale
    // one there is still refused.
    e.execute("CREATE TABLE ct(name TEXT)").unwrap();
    let msg = err(&mut e, "CREATE INDEX ON ct (name COLLATE \"en_US\")");
    assert!(msg.contains("not supported in this position"), "{msg}");
}

/// The half the message now points at: declared on a column, a locale
/// collation is performed, and it is performed the way PG performs it.
///
/// Every value here was read from PG 18.4 with the same declaration.
#[test]
fn round670_a_declared_locale_collation_is_performed() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cl(x TEXT COLLATE \"en_US.utf8\", y TEXT)")
        .unwrap();
    e.execute("INSERT INTO cl VALUES ('Zebra','Zebra'),('apple','apple'),('DateStyle','DateStyle'),('client','client')")
        .unwrap();
    assert_eq!(
        one(&mut e, "SELECT string_agg(x, ' ' ORDER BY x) FROM cl"),
        "apple client DateStyle Zebra"
    );
    assert_eq!(one(&mut e, "SELECT min(x) FROM cl"), "apple");
    assert_eq!(one(&mut e, "SELECT count(*) FROM cl WHERE x < 'b'"), "1");
    // The column beside it, with no clause, still sorts by bytes --
    // SPG's DATABASE collation is `C`. PG 18.4 disagrees here and only
    // here, because that oracle's database collates as `en_US.utf8`;
    // see docs/FINDING-2026-08-23-database-collation.md.
    assert_eq!(
        one(&mut e, "SELECT string_agg(y, ' ' ORDER BY y) FROM cl"),
        "DateStyle Zebra apple client"
    );
}
