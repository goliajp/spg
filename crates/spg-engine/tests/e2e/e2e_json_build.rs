//! v7.17.0 Phase 3.P0-28 — PG JSON builders.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-json.html
//!
//! Surface covered:
//!   * `to_json(v)` / `to_jsonb(v)` — coerce any value to JSON.
//!   * `json_build_object(k, v, ...)` / `jsonb_build_object(...)`
//!     — variadic, even-length arg list of alternating key/value.
//!   * `json_build_array(...)` / `jsonb_build_array(...)`
//!     — variadic value list → JSON array.
//!   * `jsonb_set(j, path, new [, create_missing])` — replace at
//!     PG text-array path.
//!   * `jsonb_insert(j, path, new [, insert_after])` — insert at
//!     PG text-array path (array → before/after index; object →
//!     create key, error if exists).
//!
//! These are the canonical ORM / app surface for building JSON
//! payloads on the server. Django's `JSONField` `Func` wrappers,
//! Rails' `Arel::Nodes::JsonBuildObject`, and SQLAlchemy's
//! `func.json_build_object` all compile down to these.
//!
//! Invariants pinned:
//!   * to_json(NULL::int)  → SQL NULL. These functions are STRICT;
//!     the header used to claim the opposite and round 603 checked it
//!     against live PG18. A JSON `null` VALUE still passes through,
//!     and a NULL inside a builder is still the JSON `null`.
//!   * to_json(text)       → quoted JSON string (escapes honoured).
//!   * to_json(json)       → pass-through (already-valid JSON text).
//!   * json_build_object   — odd-length args → error; NULL key →
//!     error; values run through to_json.
//!   * json_build_array    — empty arg list → `[]`.
//!   * jsonb_set path      — text-array literal `'{a,b,0}'`; object
//!     step = key, array step = integer index (negative counts from
//!     end). Missing path with create_missing=true creates the leaf
//!     in the existing parent; create_missing=false → no change.
//!   * jsonb_insert        — for object parent the final key MUST
//!     NOT already exist (PG raises; we mirror as EvalError).
//!     insert_after defaults to false (insert BEFORE index).

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one_row(r: QueryResult) -> Vec<Value<'static>> {
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            rows.into_iter().next().unwrap().values
        }
        _ => panic!("expected Rows"),
    }
}

fn one_cell(eng: &mut Engine, sql: &str) -> Value<'static> {
    let row = one_row(eng.execute(sql).unwrap());
    assert_eq!(row.len(), 1, "{sql}");
    row.into_iter().next().unwrap()
}

fn json_text(v: Value<'_>) -> String {
    match v {
        Value::Json(s) => s.into_owned(),
        other => panic!("expected Value::Json, got {other:?}"),
    }
}

// ── to_json / to_jsonb ───────────────────────────────────────────

#[test]
fn to_json_int_renders_bare_number() {
    let mut e = Engine::new();
    assert_eq!(json_text(one_cell(&mut e, "SELECT to_json(42)")), "42");
}

#[test]
fn to_json_bigint_renders_bare_number() {
    let mut e = Engine::new();
    assert_eq!(
        json_text(one_cell(&mut e, "SELECT to_jsonb(9223372036854775807)")),
        "9223372036854775807"
    );
}

#[test]
fn to_json_text_quotes_and_escapes() {
    let mut e = Engine::new();
    let s = json_text(one_cell(&mut e, "SELECT to_json('hi\"there'::text)"));
    assert_eq!(s, r#""hi\"there""#);
}

#[test]
fn to_json_text_escapes_newline_and_backslash() {
    let mut e = Engine::new();
    let s = json_text(one_cell(&mut e, "SELECT to_json('a\\nb'::text)"));
    // \n in the SQL literal arrives as the two chars '\' 'n' (PG
    // string literal — not the newline escape unless E'...' prefix).
    assert_eq!(s, r#""a\\nb""#);
}

#[test]
fn to_json_bool_renders_lowercase() {
    let mut e = Engine::new();
    assert_eq!(json_text(one_cell(&mut e, "SELECT to_json(true)")), "true");
    assert_eq!(
        json_text(one_cell(&mut e, "SELECT to_json(false)")),
        "false"
    );
}

#[test]
fn to_json_of_null_is_sql_null() {
    // v7.39 (round 603) — this test used to assert the opposite, with the
    // comment "PG: SELECT to_json(NULL::int) → 'null'::json (NOT SQL NULL)".
    // That is not what PG does. Asked live, PG18 answers:
    //
    //     SELECT to_json(NULL::int) IS NULL     →  t
    //     SELECT to_jsonb(NULL::int) IS NULL    →  t
    //     SELECT to_json(NULL)                  →  ERROR: could not
    //                                              determine polymorphic type
    //     SELECT to_jsonb('null'::json)::TEXT   →  null
    //     SELECT jsonb_build_object('a', NULL)  →  {"a": null}
    //
    // The functions are STRICT: a NULL argument gives a NULL result. A JSON
    // `null` VALUE is a different thing and still passes through, and a NULL
    // inside a builder is still the JSON `null` — both below.
    //
    // SPG has no polymorphic type resolution, so the untyped spelling PG
    // rejects is answered rather than refused; that lesser divergence is in
    // the ledger.
    let mut e = Engine::new();
    assert!(matches!(
        one_cell(&mut e, "SELECT to_json(NULL::INT)"),
        Value::Null
    ));
    assert!(matches!(
        one_cell(&mut e, "SELECT to_jsonb(NULL::INT)"),
        Value::Null
    ));
    assert_eq!(
        json_text(one_cell(&mut e, "SELECT to_jsonb('null'::JSON)")),
        "null",
        "a JSON null VALUE is not a NULL argument"
    );
    assert_eq!(
        json_text(one_cell(&mut e, "SELECT jsonb_build_object('a', NULL)")),
        r#"{"a": null}"#
    );
}

#[test]
fn to_json_json_passes_through() {
    let mut e = Engine::new();
    let s = json_text(one_cell(&mut e, r#"SELECT to_jsonb('{"a": 1}'::json)"#));
    assert_eq!(s, r#"{"a": 1}"#);
}

// ── json_build_object / jsonb_build_object ───────────────────────

#[test]
fn json_build_object_basic_keys_and_values() {
    let mut e = Engine::new();
    let s = json_text(one_cell(
        &mut e,
        "SELECT json_build_object('a', 1, 'b', 'two', 'c', true)",
    ));
    assert_eq!(s, r#"{"a" : 1, "b" : "two", "c" : true}"#);
}

#[test]
fn jsonb_build_object_empty_arg_list() {
    let mut e = Engine::new();
    assert_eq!(
        json_text(one_cell(&mut e, "SELECT jsonb_build_object()")),
        "{}"
    );
}

#[test]
fn json_build_object_value_null_serialises_null() {
    let mut e = Engine::new();
    let s = json_text(one_cell(
        &mut e,
        "SELECT json_build_object('a', NULL, 'b', 1)",
    ));
    assert_eq!(s, r#"{"a" : null, "b" : 1}"#);
}

#[test]
fn json_build_object_odd_args_errors() {
    let mut e = Engine::new();
    let r = e.execute("SELECT json_build_object('a', 1, 'b')");
    assert!(r.is_err(), "odd-length arg list must error");
}

#[test]
fn json_build_object_null_key_errors() {
    let mut e = Engine::new();
    let r = e.execute("SELECT json_build_object(NULL, 1)");
    assert!(r.is_err(), "NULL key must error");
}

// ── json_build_array / jsonb_build_array ─────────────────────────

#[test]
fn json_build_array_mixed_types() {
    let mut e = Engine::new();
    let s = json_text(one_cell(
        &mut e,
        "SELECT json_build_array(1, 'x', true, NULL)",
    ));
    assert_eq!(s, r#"[1, "x", true, null]"#);
}

#[test]
fn jsonb_build_array_empty_arg_list() {
    let mut e = Engine::new();
    assert_eq!(
        json_text(one_cell(&mut e, "SELECT jsonb_build_array()")),
        "[]"
    );
}

// ── jsonb_set ────────────────────────────────────────────────────

#[test]
fn jsonb_set_replaces_existing_object_key() {
    let mut e = Engine::new();
    let s = json_text(one_cell(
        &mut e,
        r#"SELECT jsonb_set('{"a": 1, "b": 2}', '{a}', '99')"#,
    ));
    assert_eq!(s, r#"{"a": 99, "b": 2}"#);
}

#[test]
fn jsonb_set_replaces_nested_object_key() {
    let mut e = Engine::new();
    let s = json_text(one_cell(
        &mut e,
        r#"SELECT jsonb_set('{"a":{"b":{"c":1}}}', '{a,b,c}', '"x"')"#,
    ));
    assert_eq!(s, r#"{"a": {"b": {"c": "x"}}}"#);
}

#[test]
fn jsonb_set_replaces_array_index() {
    let mut e = Engine::new();
    let s = json_text(one_cell(
        &mut e,
        r#"SELECT jsonb_set('[10,20,30]', '{1}', '99')"#,
    ));
    assert_eq!(s, "[10, 99, 30]");
}

#[test]
fn jsonb_set_creates_missing_object_key_when_default_true() {
    let mut e = Engine::new();
    let s = json_text(one_cell(
        &mut e,
        r#"SELECT jsonb_set('{"a": 1}', '{b}', '2')"#,
    ));
    assert_eq!(s, r#"{"a": 1, "b": 2}"#);
}

#[test]
fn jsonb_set_missing_with_create_missing_false_returns_unchanged() {
    let mut e = Engine::new();
    let s = json_text(one_cell(
        &mut e,
        r#"SELECT jsonb_set('{"a": 1}', '{b}', '2', false)"#,
    ));
    assert_eq!(s, r#"{"a": 1}"#);
}

// ── jsonb_insert ─────────────────────────────────────────────────

#[test]
fn jsonb_insert_into_array_before_index_by_default() {
    let mut e = Engine::new();
    let s = json_text(one_cell(
        &mut e,
        r#"SELECT jsonb_insert('[10,20,30]', '{1}', '99')"#,
    ));
    assert_eq!(s, "[10, 99, 20, 30]");
}

#[test]
fn jsonb_insert_into_array_after_index_when_true() {
    let mut e = Engine::new();
    let s = json_text(one_cell(
        &mut e,
        r#"SELECT jsonb_insert('[10,20,30]', '{1}', '99', true)"#,
    ));
    assert_eq!(s, "[10, 20, 99, 30]");
}

#[test]
fn jsonb_insert_creates_new_object_key() {
    let mut e = Engine::new();
    let s = json_text(one_cell(
        &mut e,
        r#"SELECT jsonb_insert('{"a": 1}', '{b}', '"new"')"#,
    ));
    assert_eq!(s, r#"{"a": 1, "b": "new"}"#);
}

#[test]
fn jsonb_insert_errors_on_existing_object_key() {
    let mut e = Engine::new();
    let r = e.execute(r#"SELECT jsonb_insert('{"a": 1}', '{a}', '2')"#);
    assert!(r.is_err(), "jsonb_insert on existing key must error");
}
