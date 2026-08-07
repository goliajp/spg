//! v7.39 (round 238) — operator resolution. A 25-case sweep of
//! cross-type comparison against live PG18.4 (2026-07-19) found `=` and
//! its family already reporting PG's "operator does not exist: integer =
//! text" — but three constructs that compare WITHOUT going through that
//! path answered happily across types PG has no operator for:
//!
//!   * `1 IS DISTINCT FROM 'a'::text` was `true`;
//!   * `nullif(1, 'a'::text)` was `1`;
//!   * `1 IN (1, 'a'::text)` was `true` — the item-by-item loop broke on
//!     the first match, so the offending element was never reached.
//!
//! All three are predicates: a comparison PG rejects outright was quietly
//! deciding whether a row survived.
//!
//! The sweep also found three errors printing Rust `Debug` dumps of
//! internal enums (`+ applied to non-numeric: Some(Int) vs Some(Text)`,
//! `AND on non-boolean: …`, `unary - applied to Some(Text)`) where PG has
//! a phrase a driver can match on, and AND/OR short-circuiting BEFORE
//! type-checking their arguments.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Null => "NULL".to_string(),
            v => spg_engine::eval::value_to_text(v),
        },
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
fn comparing_constructs_inherit_operator_resolution() {
    let mut e = Engine::new();
    for sql in [
        "SELECT 1 IS DISTINCT FROM 'a'::text",
        "SELECT 1 IS NOT DISTINCT FROM 'a'::text",
        "SELECT nullif(1,'a'::text)",
        "SELECT 1 IN (1,'a'::text)",
        "SELECT 1 NOT IN (1,'a'::text)",
    ] {
        let got = err(&mut e, sql);
        assert!(
            got.contains("operator does not exist: integer = text"),
            "{sql}: {got}"
        );
    }
    // The comparable shapes still answer.
    assert_eq!(text(&mut e, "SELECT 1 IS DISTINCT FROM 2"), "true");
    assert_eq!(text(&mut e, "SELECT 1 IS NOT DISTINCT FROM 1"), "true");
    assert_eq!(text(&mut e, "SELECT nullif(1,1)"), "NULL");
    assert_eq!(text(&mut e, "SELECT nullif(1,2)"), "1");
    assert_eq!(text(&mut e, "SELECT nullif(1,'2')"), "1");
    assert_eq!(text(&mut e, "SELECT 1 IN (1,'2')"), "true");
    assert_eq!(text(&mut e, "SELECT 1 IN (2,3)"), "false");
    // NULL is untyped: SPG has no `Unknown` DataType, so a bare NULL
    // describes as TEXT and must not be read as a text needle.
    assert_eq!(text(&mut e, "SELECT (NULL IN (1,2))::text"), "NULL");
    assert_eq!(text(&mut e, "SELECT 1 IS DISTINCT FROM NULL"), "true");
}

#[test]
fn arithmetic_and_unary_errors_use_pgs_phrasing() {
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "SELECT 1 + 'a'::text",
            "operator does not exist: integer + text",
        ),
        (
            "SELECT 'a'::text - 1",
            "operator does not exist: text - integer",
        ),
        ("SELECT -'a'::text", "operator does not exist: - text"),
        ("SELECT - true", "operator does not exist: - boolean"),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql}\n  want {want:?}\n  got  {got:?}");
        // No Rust Debug dump of an internal enum.
        assert!(!got.contains("Some("), "internal wording leaked: {got}");
    }
    // Untyped literals still coerce, so ordinary arithmetic is untouched.
    assert_eq!(text(&mut e, "SELECT 1 + '2'"), "3");
    assert_eq!(text(&mut e, "SELECT 'a'::text || 1"), "a1");
}

#[test]
fn boolean_connectives_type_check_before_short_circuiting() {
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "SELECT true AND 1",
            "argument of AND must be type boolean, not type integer",
        ),
        (
            "SELECT false AND 1",
            "argument of AND must be type boolean, not type integer",
        ),
        (
            "SELECT 1 OR true",
            "argument of OR must be type boolean, not type integer",
        ),
        (
            "SELECT true OR 1",
            "argument of OR must be type boolean, not type integer",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql}\n  want {want:?}\n  got  {got:?}");
    }
    // Real booleans, including the three-valued cases, are unchanged.
    assert_eq!(text(&mut e, "SELECT (false AND NULL)::text"), "false");
    assert_eq!(text(&mut e, "SELECT (true AND NULL)::text"), "NULL");
    assert_eq!(text(&mut e, "SELECT (true OR NULL)::text"), "true");
    assert_eq!(text(&mut e, "SELECT (false OR NULL)::text"), "NULL");
}
