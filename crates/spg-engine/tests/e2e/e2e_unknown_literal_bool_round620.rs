//! v7.39 (round 620) — a string literal beside a boolean connective was
//! refused, and a cast target that names no type said the wrong thing.
//!
//! `SELECT 'true' AND true` is answered by PG (`t`), and so are `'f' OR
//! false` and `'yes' AND true`. All three were refused here with `argument of
//! AND must be type boolean, not type text` — the message PG reserves for an
//! operand that really IS text. Three ordinary queries that run on PG and
//! failed on SPG.
//!
//! An unadorned string literal carries PG's `unknown` type: a value whose
//! type the context chooses. A boolean connective is such a context, so the
//! literal resolves TO boolean — and when it will not parse as one, the
//! failure is the input-syntax error the coercion itself raises
//! (`invalid input syntax for type boolean: "a"`, 22P02), not a type
//! complaint. `''::TEXT AND true` is a different thing entirely: it is text
//! because it was said to be, and stays refused, with PG's wording and PG's
//! type name.
//!
//! Getting the boundary right is the whole of it — the pins below carry every
//! spelling PG accepts, the one it rejects, the explicitly-typed operand that
//! must stay refused, and the integer that was already right.
//!
//! The MySQL dialect keeps its own reading of these connectives (MariaDB
//! answers `1` for `!'abc'`), which an existing pin caught when the first cut
//! of this took the `NOT` arm ahead of the dialect's.
//!
//! Separately: ``unsupported cast target `::nosuchtype` `` reads as "SPG has
//! not got round to that one" when what happened is that no such type exists
//! anywhere. PG says `type "nosuchtype" does not exist`; saying so also moves
//! the wire code off the generic 42000 onto 42704 UNDEFINED_OBJECT.
//!
//! 12 shapes were checked against live PG18; 9 match byte for byte. The three
//! that do not are recorded rather than faked, and are older than this round:
//! `1::anyarray` wording (checklist §9, r607), a SCHEMA-qualified unknown type
//! (`1::pg_catalog.nosuchtype` reports just `pg_catalog` — the name is split
//! on the dot), and `a.nosuch` losing its qualifier in the message.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

/// Every spelling PG's boolean input accepts, through a connective.
#[test]
fn round620_an_unknown_literal_resolves_to_boolean() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT 'true' AND true", "true"),
        ("SELECT 't' AND true", "true"),
        ("SELECT 'yes' AND true", "true"),
        ("SELECT 'y' AND true", "true"),
        ("SELECT 'on' AND true", "true"),
        ("SELECT '1' AND true", "true"),
        ("SELECT 'false' OR false", "false"),
        ("SELECT 'f' OR false", "false"),
        ("SELECT 'no' OR false", "false"),
        ("SELECT 'off' OR false", "false"),
        ("SELECT '0' OR false", "false"),
        ("SELECT 'TRUE' AND true", "true"),
        ("SELECT '  true  ' AND true", "true"),
        ("SELECT NOT 'true'", "false"),
        ("SELECT NOT 'f'", "true"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

/// Both sides, and the three-valued rules the resolution must not disturb.
#[test]
fn round620_both_sides_and_the_null_rules() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 'true' AND 'false'"), "false");
    assert_eq!(one(&mut e, "SELECT 'f' OR 'yes'"), "true");
    assert_eq!(one(&mut e, "SELECT 'true' AND NULL"), "NULL");
    assert_eq!(one(&mut e, "SELECT 'false' AND NULL"), "false");
    assert_eq!(one(&mut e, "SELECT 'true' OR NULL"), "true");
    assert_eq!(one(&mut e, "SELECT 'false' OR NULL"), "NULL");
    // Round 621 closed the short circuit (checklist C07), and it composes
    // with the resolution this file is about: the literal becomes false, and
    // false decides.
    assert_eq!(one(&mut e, "SELECT 'false' AND (1/0 = 0)"), "false");
    assert_eq!(one(&mut e, "SELECT 'true' OR (1/0 = 0)"), "true");
}

/// What must NOT be resolved, and what the failure says.
#[test]
fn round620_what_stays_refused() {
    let mut e = Engine::new();
    assert!(
        err(&mut e, "SELECT 'a' AND true")
            .contains(r#"invalid input syntax for type boolean: "a""#),
        "an unparseable literal is the coercion's own error, not a type complaint: {}",
        err(&mut e, "SELECT 'a' AND true")
    );
    assert!(
        err(&mut e, "SELECT NOT 'a'").contains(r#"invalid input syntax for type boolean: "a""#)
    );
    assert!(
        err(&mut e, "SELECT 'o' AND true")
            .contains(r#"invalid input syntax for type boolean: "o""#),
        "`o` alone is ambiguous between on and off"
    );
    assert!(
        err(&mut e, "SELECT ''::TEXT AND true")
            .contains("argument of AND must be type boolean, not type text"),
        "an operand that was SAID to be text is text"
    );
    assert!(
        err(&mut e, "SELECT 1 AND true")
            .contains("argument of AND must be type boolean, not type integer"),
        "and an integer was already right"
    );
    let mut e2 = Engine::new();
    e2.execute("CREATE TABLE bt (s TEXT)").unwrap();
    e2.execute("INSERT INTO bt VALUES ('true')").unwrap();
    assert!(
        err(&mut e2, "SELECT s AND true FROM bt")
            .contains("argument of AND must be type boolean, not type text"),
        "a text COLUMN is not an unknown literal either"
    );
}

/// The cast target that names no type.
#[test]
fn round620_unknown_cast_target_says_the_type_does_not_exist() {
    let mut e = Engine::new();
    for sql in [
        "SELECT 1::nosuchtype_zz",
        "SELECT CAST(1 AS nosuchtype_zz)",
        "SELECT NULL::nosuchtype_zz",
    ] {
        assert!(
            err(&mut e, sql).contains(r#"type "nosuchtype_zz" does not exist"#),
            "{sql} said: {}",
            err(&mut e, sql)
        );
    }
    assert_eq!(
        one(&mut e, "SELECT 1::INT, 1::TEXT, 1::BOOLEAN"),
        "1|1|true",
        "the targets that DO resolve are untouched"
    );
}
