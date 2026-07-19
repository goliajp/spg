//! v7.39 (round 259) — the DOMAIN surface, swept 63 cases against live
//! PG18.4 (2026-07-20). Single-level CHECK / NOT NULL enforcement, the
//! cast error class, and domain columns on tables already matched. The
//! gaps:
//!
//!   * DOMAIN OVER DOMAIN silently skipped the parent's constraints.
//!     `CREATE DOMAIN child AS parent CHECK (…)` recorded only the
//!     ultimate scalar type, so a value violating the PARENT's CHECK was
//!     accepted — a data-integrity hole, not an error. PG checks the
//!     whole chain BASE-FIRST (probed: a value violating both reports
//!     the parent's constraint) and names the domain being cast TO while
//!     reporting the constraint that actually failed. An ALTER on the
//!     parent must keep affecting the child (probed), so the chain is
//!     walked at check time rather than copied at CREATE time —
//!     `DomainDef.base_domain`, catalog FILE_VERSION 74.
//!   * A DOMAIN's DEFAULT was never adopted by a column of that domain:
//!     an omitted column landed NULL where PG gives the domain default.
//!   * A COLUMN-level DEFAULT on a domain column failed the whole CREATE
//!     TABLE with "type mismatch" — a hard error on valid SQL. The
//!     column still carries the parser's Text placeholder when the
//!     default is coerced; the real type only arrives when the domain
//!     binding resolves, so the coercion moved there.
//!   * `pg_typeof` reported the BASE type. Like an enum, a domain value
//!     travels as its base type's value, so the name comes from the
//!     expression — resolved statically, and gated on the catalog (the
//!     round-258 lesson: the name resolver returns ANY named cast's
//!     target, so an ungated use hijacks `x::float8`).
//!
//! Recorded residuals, all probed: `ALTER DOMAIN … ADD/DROP CONSTRAINT`
//! is unimplemented (no AST, no execution — the statement is currently
//! swallowed, so a dropped constraint keeps rejecting); a cast whose
//! text does not parse as the base type reports SPG's operator error
//! rather than PG's `invalid input syntax for type integer: "x"`; and
//! `information_schema.columns.domain_name` does not exist.

use spg_engine::{Engine, QueryResult};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN pbase2 AS int CHECK (VALUE > 0)").unwrap();
    e.execute("CREATE DOMAIN pchild2 AS pbase2 CHECK (VALUE % 2 = 0)").unwrap();
    e.execute("CREATE DOMAIN wd2 AS int DEFAULT 42").unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect::<Vec<_>>()
            .join("|"),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

#[test]
fn a_domain_over_a_domain_enforces_the_whole_chain() {
    let mut e = seeded();
    // Satisfies both.
    assert_eq!(one(&mut e, "SELECT 4::pchild2"), "4");
    // Violates only the PARENT's check — this was silently accepted.
    let got = err(&mut e, "SELECT (-4)::pchild2");
    assert!(
        got.contains("value for domain pchild2 violates check constraint \"pbase2_check\""),
        "{got}"
    );
    // Violates only the child's.
    let got = err(&mut e, "SELECT 3::pchild2");
    assert!(
        got.contains("value for domain pchild2 violates check constraint \"pchild2_check\""),
        "{got}"
    );
    // Violates BOTH — the base is checked first (probed).
    let got = err(&mut e, "SELECT (-3)::pchild2");
    assert!(
        got.contains("value for domain pchild2 violates check constraint \"pbase2_check\""),
        "{got}"
    );
}

#[test]
fn pg_typeof_names_the_domain() {
    let mut e = seeded();
    assert_eq!(one(&mut e, "SELECT pg_typeof(4::pchild2)"), "pchild2");
    assert_eq!(one(&mut e, "SELECT pg_typeof(5::pbase2)"), "pbase2");
    e.execute("CREATE TABLE pt (id int, p pbase2)").unwrap();
    e.execute("INSERT INTO pt VALUES (1, 5)").unwrap();
    assert_eq!(one(&mut e, "SELECT pg_typeof(p) FROM pt LIMIT 1"), "pbase2");
    // The gate holds: a builtin cast still reports PG's spelling, not
    // the cast's literal target text.
    assert_eq!(one(&mut e, "SELECT pg_typeof(1::float8)"), "double precision");
    assert_eq!(one(&mut e, "SELECT pg_typeof(1::int2)"), "smallint");
}

#[test]
fn domain_and_column_defaults_both_apply() {
    let mut e = seeded();
    // `v` carries a COLUMN default that overrides the domain's; creating
    // this table used to fail outright with "type mismatch".
    e.execute("CREATE TABLE pt (id int, w wd2, v wd2 DEFAULT 7, p pbase2)")
        .unwrap();
    e.execute("INSERT INTO pt (id, p) VALUES (1, 5)").unwrap();
    assert_eq!(one(&mut e, "SELECT w, v FROM pt WHERE id=1"), "42|7");
    // An EXPLICIT NULL stays NULL — the default only fills an omission.
    e.execute("INSERT INTO pt (id, w, p) VALUES (2, NULL, 5)").unwrap();
    assert_eq!(one(&mut e, "SELECT w FROM pt WHERE id=2"), "NULL");
    // The domain's CHECK still guards the column.
    let got = err(&mut e, "INSERT INTO pt (id, p) VALUES (3, -1)");
    assert!(got.contains("value for domain pbase2"), "{got}");
    assert_eq!(one(&mut e, "SELECT count(*) FROM pt"), "2");
}

#[test]
fn the_single_level_domain_core_is_unchanged() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN posint AS int CHECK (VALUE > 0)").unwrap();
    e.execute("CREATE DOMAIN shortname AS text NOT NULL CHECK (length(VALUE) <= 5)")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT 5::posint"), "5");
    assert_eq!(one(&mut e, "SELECT 'abc'::shortname"), "abc");
    for (sql, want) in [
        ("SELECT (-1)::posint", "value for domain posint violates check constraint"),
        ("SELECT 0::posint", "value for domain posint violates check constraint"),
        (
            "SELECT 'toolongvalue'::shortname",
            "value for domain shortname violates check constraint",
        ),
        ("SELECT NULL::shortname", "domain shortname does not allow null values"),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql} → {got}");
    }
    // Arithmetic on a domain value works on the base type.
    assert_eq!(one(&mut e, "SELECT 5::posint + 1"), "6");
    assert_eq!(one(&mut e, "SELECT pg_typeof(5::posint + 1)"), "integer");
}
