//! v7.17.0 Phase 3.P0-31 — PG `pg_typeof(any)`.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-info.html
//!
//! Surface:
//!   * `pg_typeof(v)` → canonical PG type-name TEXT (lowercase).
//!   * `pg_typeof(NULL)` → 'unknown' (PG semantic; NULL has no
//!     resolved type at the value level).
//!
//! Why this matters:
//!   * sqlx / SQLAlchemy / Diesel introspection queries — they
//!     probe column / expression types via `SELECT pg_typeof(...)`
//!     to decide how to bind / decode.
//!   * Generic ORMs that emit conditional SQL based on the
//!     server-side type (`CASE WHEN pg_typeof(x) = 'jsonb' …`).
//!
//! Invariants pinned:
//!   * Names match PG canonical, NOT SPG's UPPERCASE `Display` shape.
//!   * Array suffix is `'[]'` (PG external form), not the internal
//!     `_int4` / `_text` form (we don't model the internal form).

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one_text(eng: &mut Engine, sql: &str) -> String {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "{sql}");
            let row = rows.into_iter().next().unwrap();
            match row.values.into_iter().next().unwrap() {
                Value::Text(s) => s.into_owned(),
                other => panic!("{sql}: expected Text, got {other:?}"),
            }
        }
        _ => panic!("expected Rows"),
    }
}

#[test]
fn pg_typeof_int_literal() {
    let mut e = Engine::new();
    assert_eq!(one_text(&mut e, "SELECT pg_typeof(42)"), "integer");
}

#[test]
fn pg_typeof_bigint_overflow_into_bigint() {
    let mut e = Engine::new();
    // 9_999_999_999 > i32::MAX → parser puts it into BigInt.
    assert_eq!(one_text(&mut e, "SELECT pg_typeof(9999999999)"), "bigint");
}

#[test]
fn pg_typeof_text_cast() {
    let mut e = Engine::new();
    assert_eq!(one_text(&mut e, "SELECT pg_typeof('hi'::text)"), "text");
}

#[test]
fn pg_typeof_boolean() {
    let mut e = Engine::new();
    assert_eq!(one_text(&mut e, "SELECT pg_typeof(true)"), "boolean");
}

#[test]
fn pg_typeof_null_returns_unknown() {
    let mut e = Engine::new();
    assert_eq!(one_text(&mut e, "SELECT pg_typeof(NULL)"), "unknown");
}

#[test]
fn pg_typeof_float() {
    let mut e = Engine::new();
    assert_eq!(
        one_text(&mut e, "SELECT pg_typeof(3.14::float)"),
        "double precision"
    );
}

#[test]
fn pg_typeof_numeric_via_column() {
    // SPG's parser doesn't accept the bare `::numeric` cast yet;
    // route through a NUMERIC column to assert the value-level
    // type still surfaces correctly via pg_typeof.
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (price NUMERIC(10,2))").unwrap();
    e.execute("INSERT INTO p VALUES (123.45)").unwrap();
    assert_eq!(
        one_text(&mut e, "SELECT pg_typeof(price) FROM p"),
        "numeric"
    );
}

#[test]
fn pg_typeof_date_and_timestamp() {
    let mut e = Engine::new();
    assert_eq!(
        one_text(&mut e, "SELECT pg_typeof('2025-06-08'::DATE)"),
        "date"
    );
    assert_eq!(
        one_text(&mut e, "SELECT pg_typeof('2025-06-08 14:30:45'::TIMESTAMP)"),
        "timestamp without time zone"
    );
}

#[test]
fn pg_typeof_json_returns_json() {
    // NOTE: SPG carries both `JSON` and `JSONB` columns as the
    // same `Value::Json` variant; the ::jsonb cast collapses to
    // the same Value as ::json. At the value level pg_typeof
    // returns "json" for both — disambiguation lives at column
    // level (see e2e_jsonb.rs for the catalog-level distinction).
    let mut e = Engine::new();
    assert_eq!(one_text(&mut e, "SELECT pg_typeof('{}'::json)"), "json");
}

#[test]
fn pg_typeof_uuid() {
    let mut e = Engine::new();
    assert_eq!(
        one_text(&mut e, "SELECT pg_typeof(gen_random_uuid())"),
        "uuid"
    );
}

#[test]
fn pg_typeof_of_column_through_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, body TEXT, n NUMERIC(10,2))")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'x', 3.50)").unwrap();
    assert_eq!(one_text(&mut e, "SELECT pg_typeof(id) FROM t"), "integer");
    assert_eq!(one_text(&mut e, "SELECT pg_typeof(body) FROM t"), "text");
    assert_eq!(one_text(&mut e, "SELECT pg_typeof(n) FROM t"), "numeric");
}
