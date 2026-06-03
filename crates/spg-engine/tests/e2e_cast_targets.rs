//! v7.9.25 — `::INTERVAL`, `::JSON`, `::JSONB`, `::TIMESTAMPTZ` cast
//! targets. v7.9.26 — `::regtype` / `::regclass` accept-then-fail
//! cleanly. mailrs migration follow-up H3.

use spg_engine::{Engine, EngineError, QueryResult};
use spg_storage::Value;

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        eng.execute(sql).unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
    }
    eng
}

#[test]
fn interval_cast_from_text_literal() {
    let mut eng = Engine::new();
    let r = eng.execute("SELECT '7 days'::INTERVAL AS span").unwrap();
    let QueryResult::Rows { rows, .. } = r else { panic!() };
    assert!(matches!(rows[0].values[0], Value::Interval { .. }));
}

#[test]
fn interval_cast_arithmetic_with_now() {
    // mailrs ICS feed worker shape: `next_poll <= now() - ($1::INTERVAL)`.
    let mut eng = Engine::new();
    let r = eng
        .execute("SELECT '1 day'::INTERVAL AS one_day")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else { panic!() };
    let Value::Interval { months, micros } = rows[0].values[0] else { panic!() };
    assert_eq!(months, 0);
    assert_eq!(micros, 86_400_000_000);
}

#[test]
fn jsonb_cast_from_text() {
    let mut eng = Engine::new();
    let r = eng
        .execute(r#"SELECT '{"k":"v"}'::JSONB AS payload"#)
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else { panic!() };
    let Value::Json(s) = &rows[0].values[0] else { panic!("{:?}", rows[0].values[0]) };
    assert_eq!(s, r#"{"k":"v"}"#);
}

#[test]
fn json_cast_is_alias_for_jsonb_runtime() {
    let mut eng = Engine::new();
    let r = eng.execute(r#"SELECT '42'::JSON AS n"#).unwrap();
    let QueryResult::Rows { rows, .. } = r else { panic!() };
    assert!(matches!(rows[0].values[0], Value::Json(_)));
}

#[test]
fn timestamptz_cast_works_like_timestamp() {
    let mut eng = Engine::new();
    let r = eng
        .execute("SELECT '2026-06-04 12:00:00'::TIMESTAMPTZ AS t")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else { panic!() };
    assert!(matches!(rows[0].values[0], Value::Timestamp(_)));
}

#[test]
fn regtype_cast_surfaces_clear_unsupported_error() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
    ]);
    // Parses cleanly, errors at eval with a hint pointing to SPG
    // alternatives.
    let r = eng.execute("SELECT 'public.t'::REGCLASS AS oid");
    assert!(matches!(r, Err(EngineError::Eval(_))));
}

#[test]
fn regtype_cast_does_not_lex_fail() {
    // The old error was "expected type ident after `::`, got
    // Ident(\"regtype\")". Now it parses; failure (if any) is at
    // execute time, not parse time.
    let mut eng = Engine::new();
    let stmt = eng.prepare("SELECT 'x'::REGTYPE");
    assert!(stmt.is_ok(), "regtype must parse cleanly");
}
