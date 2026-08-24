//! v7.38.19 — a pseudo-type is refused as an invalid table definition,
//! and reported by the name PostgreSQL reports.
//!
//! A pseudo-type has no storage: `cstring`, `record`, `void`,
//! `anyelement` and the rest exist to describe function signatures. PG
//! refuses a column declared with one as `column "c" has pseudo-type
//! cstring` — SQLSTATE 42P16, an INVALID TABLE DEFINITION. SPG said
//! `type "cstring" does not exist`, which is the wrong class and the
//! wrong claim: the name exists.
//!
//! Casting to one is allowed and the value travels as text, so
//! `pg_typeof` has to read the name off the EXPRESSION — the same
//! discipline enums and domains already needed here. What it reports is
//! not always the name written, and that is measured rather than
//! reasoned about: on PostgreSQL 18.4 `cstring` and `void` report
//! themselves, while `anyelement`, `anynonarray` and `unknown` all
//! report `unknown`, a polymorphic placeholder having nothing to
//! resolve against.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(t) => t.to_string(),
            other => format!("{other:?}"),
        },
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(other) => panic!("expected {sql} to be refused, got {other:?}"),
    }
}

/// Every answer is PostgreSQL 18.4's, run against the same statement.
#[test]
fn pg_typeof_reports_what_postgresql_reports() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_typeof('x'::cstring)"), "cstring");
    assert_eq!(one(&mut e, "SELECT pg_typeof('x'::void)"), "void");
    assert_eq!(one(&mut e, "SELECT pg_typeof('x'::anyelement)"), "unknown");
    assert_eq!(one(&mut e, "SELECT pg_typeof('x'::anynonarray)"), "unknown");
    assert_eq!(one(&mut e, "SELECT pg_typeof('x'::unknown)"), "unknown");
}

/// The value is unchanged by the cast, and a further cast to a real
/// type reports that real type — so the pseudo-name is not sticky.
#[test]
fn the_value_still_travels_as_text() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 'x'::cstring"), "x");
    assert_eq!(one(&mut e, "SELECT pg_typeof('x'::cstring::text)"), "text");
    assert_eq!(one(&mut e, "SELECT pg_typeof('x'::text)"), "text");
}

/// The refusal names the COLUMN and the pseudo-type, which is what
/// makes it 42P16 rather than 42704.
#[test]
fn a_column_cannot_be_declared_with_one() {
    for ty in [
        "cstring",
        "record",
        "void",
        "anyelement",
        "internal",
        "trigger",
    ] {
        let mut e = Engine::new();
        let got = err(&mut e, &format!("CREATE TABLE t (c {ty})"));
        assert!(
            got.contains(&format!("column \"c\" has pseudo-type {ty}")),
            "{ty}: {got}"
        );
        assert!(
            !got.contains("does not exist"),
            "{ty} reported as undefined, which is the class this closes: {got}"
        );
    }
}

/// A real type that does not exist is still the OTHER refusal. An
/// allowlist nothing falls outside is not a check.
#[test]
fn a_name_that_is_not_a_pseudo_type_is_still_undefined() {
    let mut e = Engine::new();
    let got = err(&mut e, "CREATE TABLE t (c nosuchtype)");
    assert!(got.contains("type \"nosuchtype\" does not exist"), "{got}");
    assert!(!got.contains("pseudo-type"), "{got}");
}
