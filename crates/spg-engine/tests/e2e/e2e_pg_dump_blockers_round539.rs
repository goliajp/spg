//! v7.39 (round 539) — what stopped `pg_dump` before it started.
//!
//! The last two rounds kept pointing at DDL round-trip fidelity, so this
//! one ran the real thing: `pg_dump --schema-only` against SPG over the
//! wire, next to the same dump from PG18. It did not get as far as
//! producing DDL — its first catalog query failed, and each fix revealed
//! the next.
//!
//!     AND c.relname OPERATOR(pg_catalog.~) '^(t)$' COLLATE pg_catalog.default
//!
//! Three separate gaps in that one line:
//!
//!   * `OPERATOR(pg_catalog.~)` — the explicit-operator spelling worked
//!     for `=` and `+` but not for the regex family, because those lower
//!     onto function calls rather than a BinOp and the fallback path read
//!     the word OPERATOR instead of the operator it names.
//!   * `COLLATE pg_catalog.default` — a SCHEMA-QUALIFIED collation. Only
//!     one token was read, so the SCHEMA became the name and the clause
//!     was refused as an unsupported locale collation.
//!   * `default` lexes as a KEYWORD, not an identifier — round 535's trap
//!     for the fourth time, after TABLE, INDEX and FULL.
//!
//! And `pg_extension` published four of PG's eight columns, with
//! `extnamespace` holding the schema's NAME where PG holds its OID —
//! which is what pg_dump joins on.
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

/// The regex family through the explicit-operator spelling.
#[test]
fn round539_operator_spelling_covers_the_symbol_family() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT 'a' OPERATOR(pg_catalog.~) '^a'"),
        "true"
    );
    assert_eq!(
        text(&mut e, "SELECT 'a' OPERATOR(pg_catalog.!~) '^b'"),
        "true"
    );
    assert_eq!(
        text(&mut e, "SELECT 'AB' OPERATOR(pg_catalog.~*) '^ab'"),
        "true"
    );
    assert_eq!(
        text(&mut e, "SELECT 'ab' OPERATOR(pg_catalog.^@) 'a'"),
        "true"
    );
    // The ones that already worked keep working.
    assert_eq!(text(&mut e, "SELECT 1 OPERATOR(pg_catalog.=) 1"), "true");
    assert_eq!(text(&mut e, "SELECT 2 OPERATOR(pg_catalog.+) 3"), "5");
    // Unqualified, too.
    assert_eq!(text(&mut e, "SELECT 'a' OPERATOR(~) '^a'"), "true");
}

/// A schema-qualified collation, whose name is a keyword.
#[test]
fn round539_schema_qualified_collation() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT 'a' = 'a' COLLATE pg_catalog.default"),
        "true"
    );
    assert_eq!(
        text(&mut e, r#"SELECT 'a' = 'a' COLLATE pg_catalog."C""#),
        "true"
    );
    // Both gaps in one predicate, which is how pg_dump writes it.
    assert_eq!(
        text(
            &mut e,
            "SELECT 'a' OPERATOR(pg_catalog.~) '^a' COLLATE pg_catalog.default"
        ),
        "true"
    );
    // v7.39.2 — this asserted that a locale collation is refused,
    // qualified or not. `Expr::Collate` carries it now and the answers
    // are PostgreSQL 18.6's, measured: `pg_catalog."en_US"` is `t` and
    // `pg_catalog."C"` on a comparison is `f`.
    assert_eq!(
        text(&mut e, r#"SELECT 'a' = 'a' COLLATE pg_catalog."en_US""#),
        "true"
    );
    assert_eq!(
        text(&mut e, r#"SELECT 'a' < 'B' COLLATE pg_catalog."C""#),
        "false",
        "byte order through a qualified name is still byte order"
    );
    // The QUALIFIER is dropped — SPG is single-schema — but it is read
    // first. PG answers `schema "nosuch_schema" does not exist`, and
    // dropping it unread meant a name that names nothing was accepted.
    assert!(
        e.execute(r#"SELECT 'a' = 'a' COLLATE nosuch_schema."C""#)
            .unwrap_err()
            .to_string()
            .contains(r#"schema "nosuch_schema" does not exist"#)
    );
}

/// `pg_extension` publishes PG's eight columns, and `extnamespace` is
/// the oid a join can use.
#[test]
fn round539_pg_extension_has_pgs_shape() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT extname, extowner, extnamespace, extrelocatable, extversion \
             FROM pg_extension WHERE extname = 'plpgsql'"
        ),
        "plpgsql|10|11|false|1.0"
    );
    // The two array columns exist and are NULL, as PG's are for an
    // extension with no configuration tables.
    assert_eq!(
        text(
            &mut e,
            "SELECT extconfig, extcondition FROM pg_extension WHERE extname = 'plpgsql'"
        ),
        "NULL|NULL"
    );
    // And the join pg_dump makes now finds its row.
    assert_eq!(
        text(
            &mut e,
            "SELECT n.nspname FROM pg_extension x JOIN pg_namespace n \
             ON n.oid = x.extnamespace WHERE x.extname = 'plpgsql'"
        ),
        "pg_catalog"
    );
}
