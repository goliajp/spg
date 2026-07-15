//! v7.39 (read01 round 116) — `pg_typeof` of a bare string literal is `unknown`.
//!
//! `pg_typeof('x')` reported `text`, but a bare, uncoerced string literal is
//! PG's `unknown` type — it stays unknown until context coerces it. A cast
//! (`'x'::text`), a concatenation, or a function argument each force text and
//! must still report `text`. Locked byte-identical against PG 18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Null => "NULL".to_string(),
            v => spg_engine::eval::value_to_text(v),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn bare_string_literal_is_unknown() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_typeof('x')::text"), "unknown");
    assert_eq!(one(&mut e, "SELECT pg_typeof('123')::text"), "unknown");
    assert_eq!(one(&mut e, "SELECT pg_typeof('2024-01-01')::text"), "unknown");
}

#[test]
fn coerced_string_is_text() {
    let mut e = Engine::new();
    // A cast, a concatenation, or a function argument forces text.
    assert_eq!(one(&mut e, "SELECT pg_typeof('x'::text)::text"), "text");
    assert_eq!(one(&mut e, "SELECT pg_typeof('x' || 'y')::text"), "text");
    assert_eq!(one(&mut e, "SELECT pg_typeof(lower('X'))::text"), "text");
}

#[test]
fn other_literals_unchanged() {
    let mut e = Engine::new();
    // Regression: NULL stays unknown; typed literals keep their own type.
    assert_eq!(one(&mut e, "SELECT pg_typeof(NULL)::text"), "unknown");
    assert_eq!(one(&mut e, "SELECT pg_typeof(1)::text"), "integer");
    assert_eq!(one(&mut e, "SELECT pg_typeof(1.5)::text"), "numeric");
    assert_eq!(one(&mut e, "SELECT pg_typeof(true)::text"), "boolean");
}
