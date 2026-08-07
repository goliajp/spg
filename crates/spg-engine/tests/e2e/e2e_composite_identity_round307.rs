//! v7.39 (round 307, V25) — composite identity survives a projection.
//!
//! `SELECT (r).nosuch FROM (SELECT ROW('x'::text,9)::rc9 AS r) s` answered
//! the ANONYMOUS-record wording, `could not identify column "nosuch" in
//! record data type`, where PG names the type: `column "nosuch" not found
//! in data type rc9`. Round 285 got the three wordings right when the base
//! was written as a cast, but a projection hands the next query level a
//! plain COLUMN, and that path never asked the catalog.
//!
//! The fix is to ask once, at the error site, which already holds the
//! catalog: find the base's declared type name — from the cast or from the
//! column's schema — and let the catalog say what it is. That also settles
//! the other half PG words differently: field notation applied to a DOMAIN
//! or an ENUM names the type too, rather than answering generically.
//!
//! All expectations read off live PG 18.4 (2026-07-21).

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    for s in [
        "CREATE TYPE rc9 AS (a text, b int)",
        "CREATE DOMAIN pos9 AS int",
        "CREATE TYPE mood9 AS ENUM ('sad','ok','glad')",
    ] {
        e.execute(s).unwrap_or_else(|x| panic!("{s}: {x:?}"));
    }
    e
}

const COMPOSITE: &str = "column \"nosuch\" not found in data type rc9";
const ANONYMOUS: &str = "could not identify column \"nosuch\" in record data type";

/// The type name has to survive every way a composite can reach the next
/// query level, because each one hands on a plain column and it is the
/// column path that used to drop it.
#[test]
fn a_composite_keeps_its_name_through_every_projection_shape() {
    let mut e = fixture();
    e.execute("CREATE TABLE tc9 (r rc9)").unwrap();
    e.execute("INSERT INTO tc9 VALUES (ROW('x'::text,9)::rc9)")
        .unwrap();
    for sql in [
        // derived table
        "SELECT (r).nosuch FROM (SELECT ROW('x'::text,9)::rc9 AS r) s",
        // two levels of derived table
        "SELECT (r).nosuch FROM (SELECT r FROM (SELECT ROW('x'::text,9)::rc9 AS r) s1) s2",
        // CTE
        "WITH c AS (SELECT ROW('x'::text,9)::rc9 AS r) SELECT (r).nosuch FROM c",
        // UNION peer
        "SELECT (r).nosuch FROM \
         (SELECT ROW('x'::text,9)::rc9 AS r UNION ALL SELECT ROW('y'::text,8)::rc9) s",
        // a real table column
        "SELECT (r).nosuch FROM tc9",
        // and the spelling round 285 already handled — still right
        "SELECT (ROW('x'::text,9)::rc9).nosuch",
    ] {
        assert!(
            err(&mut e, sql).contains(COMPOSITE),
            "{sql}\n  got: {}",
            err(&mut e, sql)
        );
    }
}

/// Naming a type must not swallow the case where there is no name to
/// give: an anonymous ROW still gets PG's record wording.
#[test]
fn an_anonymous_row_keeps_the_record_wording() {
    let mut e = fixture();
    assert!(err(&mut e, "SELECT (ROW('x'::text,9)).nosuch").contains(ANONYMOUS));
    assert!(
        err(
            &mut e,
            "SELECT (r).nosuch FROM (SELECT ROW('x'::text,9) AS r) s"
        )
        .contains(ANONYMOUS)
    );
}

/// PG words field notation on a NON-composite named type differently, and
/// names the type. A domain and an enum read the same way.
#[test]
fn field_notation_on_a_domain_or_enum_names_the_type() {
    let mut e = fixture();
    assert!(
        err(&mut e, "SELECT (d).nosuch FROM (SELECT 5::pos9 AS d) s").contains(
            "column notation .nosuch applied to type pos9, which is not a composite type"
        )
    );
    assert!(
        err(&mut e, "SELECT (m).nosuch FROM (SELECT 'ok'::mood9 AS m) s").contains(
            "column notation .nosuch applied to type mood9, which is not a composite type"
        )
    );
}

/// A field access that WORKS must keep working, and the identity that
/// `pg_typeof` reports is unchanged for all three kinds of named type.
#[test]
fn resolving_fields_and_pg_typeof_are_unchanged() {
    let mut e = fixture();
    assert_eq!(
        one(
            &mut e,
            "SELECT (r).a FROM (SELECT ROW('x'::text,9)::rc9 AS r) s"
        ),
        "x"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_typeof(r) FROM (SELECT ROW('x'::text,9)::rc9 AS r) s"
        ),
        "rc9"
    );
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(d) FROM (SELECT 5::pos9 AS d) s"),
        "pos9"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_typeof(m) FROM (SELECT 'ok'::mood9 AS m) s"
        ),
        "mood9"
    );
}

/// The name of a `::rc9` cast is currently filed under the projection's
/// ENUM slot — `expr_enum_type_name` answers for any named cast without
/// asking the catalog. Harmless because every consumer classifies against
/// the catalog, but enum ordering is the thing it would break first, so
/// pin it: member order, not label order (mood9 is sad < ok < glad).
#[test]
fn enum_member_ordering_still_holds_through_a_projection() {
    let mut e = fixture();
    e.execute("CREATE TABLE tm9 (m mood9)").unwrap();
    e.execute("INSERT INTO tm9 VALUES ('glad'),('sad'),('ok')")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT m FROM tm9 ORDER BY m"), "sad");
    assert_eq!(
        one(&mut e, "SELECT m FROM (SELECT m FROM tm9) s ORDER BY m"),
        "sad"
    );
    assert_eq!(one(&mut e, "SELECT min(m) FROM tm9"), "sad");
}
