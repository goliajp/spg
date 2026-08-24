//! v7.9.25 — `::INTERVAL`, `::JSON`, `::JSONB`, `::TIMESTAMPTZ` cast
//! targets. v7.9.26 — `::regtype` / `::regclass` accept-then-fail
//! cleanly. mailrs migration follow-up H3.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        eng.execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
    }
    eng
}

#[test]
fn interval_cast_from_text_literal() {
    let mut eng = Engine::new();
    let r = eng.execute("SELECT '7 days'::INTERVAL AS span").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert!(matches!(rows[0].values[0], Value::Interval { .. }));
}

#[test]
fn interval_cast_arithmetic_with_now() {
    // mailrs ICS feed worker shape: `next_poll <= now() - ($1::INTERVAL)`.
    let mut eng = Engine::new();
    let r = eng.execute("SELECT '1 day'::INTERVAL AS one_day").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    let Value::Interval {
        months,
        days,
        micros,
        kind,
    } = rows[0].values[0]
    else {
        panic!()
    };
    // v7.37.5 β — `'1 day'` lands in `days`, not `micros`.
    assert_eq!(months, 0);
    assert_eq!(days, 1);
    assert_eq!(micros, 0);
}

#[test]
fn jsonb_cast_from_text() {
    let mut eng = Engine::new();
    let r = eng
        .execute(r#"SELECT '{"k":"v"}'::JSONB AS payload"#)
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    let Value::Json(s) = &rows[0].values[0] else {
        panic!("{:?}", rows[0].values[0])
    };
    assert_eq!(s, r#"{"k": "v"}"#);
}

#[test]
fn json_cast_is_alias_for_jsonb_runtime() {
    let mut eng = Engine::new();
    let r = eng.execute(r#"SELECT '42'::JSON AS n"#).unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert!(matches!(rows[0].values[0], Value::Json(_)));
}

#[test]
fn timestamptz_cast_works_like_timestamp() {
    let mut eng = Engine::new();
    let r = eng
        .execute("SELECT '2026-06-04 12:00:00'::TIMESTAMPTZ AS t")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert!(matches!(rows[0].values[0], Value::Timestamp(_)));
}

#[test]
fn regclass_cast_strips_schema_prefix() {
    // v7.17.0 Phase 5.3 — `::regclass` accepts TEXT and returns
    // the bare table name (search_path-aware rendering); SPG is
    // single-schema so the `public.` prefix is always droppable.
    let mut eng = engine_with(&["CREATE TABLE t (id INT NOT NULL)"]);
    let r = eng.execute("SELECT 'public.t'::REGCLASS AS oid").unwrap();
    let spg_engine::QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    // v7.39 (read01 ruleutils.c) — regclass is dual-shape (oid + name);
    // it renders as the bare name.
    assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[0]), "t");
    assert!(matches!(rows[0].values[0], Value::RegClass(oid, _) if oid >= 16384));
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

#[test]
fn regtype_text_in_text_out() {
    let mut eng = Engine::new();
    let r = eng.execute("SELECT 'int4'::REGTYPE").unwrap();
    let spg_engine::QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    // v7.39 (read01 regproc.c) — regtype canonicalizes ('int4' → 'integer').
    // v7.39 (round 648) — assert the RENDERED value, the way the
    // regclass test below already does. This compared the Value variant
    // and so pinned `Text("integer")` — the very shape that was the
    // defect: a regtype with no oid half could not cast on to one.
    assert_eq!(
        spg_engine::eval::value_to_text(&rows[0].values[0]),
        "integer"
    );
}

#[test]
fn regclass_passes_unqualified_name_through() {
    let mut eng = engine_with(&["CREATE TABLE t (id INT NOT NULL)"]);
    let r = eng.execute("SELECT 't'::REGCLASS").unwrap();
    let spg_engine::QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    // v7.39 (read01 ruleutils.c) — dual-shape; renders as the name.
    assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[0]), "t");
}

#[test]
fn regclass_integer_oid_renders_as_text() {
    // PG path: `SELECT 16384::regclass` — integer rendered as
    // textual OID. SPG mirrors the textual contract.
    let mut eng = Engine::new();
    let r = eng.execute("SELECT 16384::REGCLASS").unwrap();
    let spg_engine::QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows[0].values[0], Value::text("16384"));
}

#[test]
fn int_text_underscore_only_between_digits() {
    // PG accepts `_` group separators only between two digits (and right
    // after a 0x/0o/0b radix prefix); a leading/trailing/doubled
    // underscore is "invalid input syntax". Live-PG18.4-verified.
    let mut eng = Engine::new();
    let ok = |eng: &mut Engine, sql: &str| -> i64 {
        let r = eng.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        let QueryResult::Rows { rows, .. } = r else {
            panic!()
        };
        match &rows[0].values[0] {
            Value::Int(n) => i64::from(*n),
            Value::BigInt(n) => *n,
            o => panic!("{sql}: {o:?}"),
        }
    };
    // Valid: between digits, hex with grouping, hex with underscore right
    // after the prefix.
    assert_eq!(ok(&mut eng, "SELECT '1_000'::int4"), 1000);
    assert_eq!(ok(&mut eng, "SELECT '1_000_000'::int8"), 1_000_000);
    assert_eq!(ok(&mut eng, "SELECT '0xFF_FF'::int4"), 65535);
    assert_eq!(ok(&mut eng, "SELECT '0x_FF'::int4"), 255);
    // Invalid: leading / trailing / doubled underscore.
    for bad in [
        "SELECT '_5'::int4",
        "SELECT '5_'::int4",
        "SELECT '123__4'::int4",
        "SELECT '0xFF_'::int4",
    ] {
        assert!(eng.execute(bad).is_err(), "{bad} must be rejected");
    }
}
