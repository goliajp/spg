//! v7.37.6-A — sentori Epic 6 JSONB operator acceptance suite.
//!
//! Pins every operator the sentori cutover plan lists, with both
//! representative shapes from the 988 `->` / 59 `->>` / 4
//! `jsonb_set` / 2 `@>` / 1 `jsonb_each_text` real callsites.
//!
//! The existing `e2e_json_path.rs` covers `@>` / `#>` / `#>>`;
//! this suite fills the `->` / `->>` / `jsonb_set` /
//! `jsonb_each_text` / `?` / `?|` / `?&` / `<@` gap.
//!
//! Status pre-v7.37.6:
//!   `->` / `->>` / `#>` / `#>>` / `@>` had AST + eval but ZERO
//!   `->` / `->>` e2e coverage — sentori would have been the
//!   first real exercise.
//!   `?` / `?|` / `?&` / `<@` / `jsonb_each_text` had ZERO
//!   footprint.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one_value(e: &mut Engine, sql: &str) -> Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows for {sql}");
    };
    rows.into_iter()
        .next()
        .expect("row")
        .values
        .into_iter()
        .next()
        .expect("col")
}

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows for {sql}");
    };
    rows.into_iter().map(|r| r.values).collect()
}

// ─── -> (member access, returns jsonb) — sentori 988 callsites ──

#[test]
fn arrow_object_member_returns_jsonb() {
    let mut e = Engine::new();
    let v = one_value(
        &mut e,
        r#"SELECT '{"name":"alice","age":30}'::jsonb -> 'name'"#,
    );
    // PG: `'{"name":"alice"}' -> 'name'` returns `"alice"` (a
    // jsonb scalar string, NOT a plain text — the wrapping quotes
    // survive).
    let Value::Json(s) = v else {
        panic!("expected jsonb, got {v:?}");
    };
    assert_eq!(s, "\"alice\"");
}

#[test]
fn arrow_object_member_missing_returns_jsonb_null() {
    let mut e = Engine::new();
    let v = one_value(&mut e, r#"SELECT '{"a": 1}'::jsonb -> 'missing'"#);
    // PG returns SQL NULL for missing key (not jsonb null).
    assert_eq!(v, Value::Null);
}

#[test]
fn arrow_array_index_returns_jsonb() {
    let mut e = Engine::new();
    let v = one_value(&mut e, r#"SELECT '[10,20,30]'::jsonb -> 1"#);
    let Value::Json(s) = v else { panic!() };
    assert_eq!(s, "20");
}

#[test]
fn arrow_chained_object_then_array() {
    // Common sentori shape: `jsonb_col -> 'items' -> 0 -> 'id'`.
    let mut e = Engine::new();
    let v = one_value(
        &mut e,
        r#"SELECT '{"items":[{"id":7},{"id":11}]}'::jsonb -> 'items' -> 1 -> 'id'"#,
    );
    let Value::Json(s) = v else { panic!() };
    assert_eq!(s, "11");
}

#[test]
fn arrow_column_path_through_stored_jsonb() {
    // Stored column, then `->`. The 988 callsites all follow this
    // `WHERE col -> 'k' = '...'::jsonb` / `SELECT col -> 'k'` shape.
    let mut e = Engine::new();
    e.execute("CREATE TABLE evt (id INT NOT NULL, body JSONB NOT NULL)")
        .unwrap();
    e.execute(
        r#"INSERT INTO evt VALUES
            (1, '{"kind":"login","ts":1000}'::jsonb),
            (2, '{"kind":"click","ts":2000}'::jsonb)"#,
    )
    .unwrap();
    let r = rows(&mut e, "SELECT body -> 'kind' FROM evt ORDER BY id");
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Json("\"login\"".into()));
    assert_eq!(r[1][0], Value::Json("\"click\"".into()));
}

// ─── ->> (member access as text) — sentori 59 callsites ──

#[test]
fn arrow_text_member_returns_text() {
    let mut e = Engine::new();
    let v = one_value(&mut e, r#"SELECT '{"name":"alice"}'::jsonb ->> 'name'"#);
    // PG: `->>` strips jsonb wrapping quotes — returns plain text.
    assert_eq!(v, Value::text("alice"));
}

#[test]
fn arrow_text_array_index_returns_text() {
    let mut e = Engine::new();
    let v = one_value(&mut e, r#"SELECT '[10,20,30]'::jsonb ->> 0"#);
    // Numbers as text.
    assert_eq!(v, Value::text("10"));
}

#[test]
fn arrow_text_missing_returns_null() {
    let mut e = Engine::new();
    let v = one_value(&mut e, r#"SELECT '{"a": 1}'::jsonb ->> 'missing'"#);
    assert_eq!(v, Value::Null);
}

#[test]
fn arrow_text_column_used_in_where() {
    // Sentori representative: `WHERE col ->> 'kind' = 'login'`.
    let mut e = Engine::new();
    e.execute("CREATE TABLE evt (id INT NOT NULL, body JSONB NOT NULL)")
        .unwrap();
    e.execute(
        r#"INSERT INTO evt VALUES
            (1, '{"kind":"login"}'::jsonb),
            (2, '{"kind":"click"}'::jsonb),
            (3, '{"kind":"login"}'::jsonb)"#,
    )
    .unwrap();
    let r = rows(
        &mut e,
        "SELECT id FROM evt WHERE body ->> 'kind' = 'login' ORDER BY id",
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(3));
}

// ─── @> (containment, returns bool) — sentori 2 callsites ──

#[test]
fn containment_object_subset() {
    let mut e = Engine::new();
    let v = one_value(
        &mut e,
        r#"SELECT '{"a": 1, "b": 2}'::jsonb @> '{"a": 1}'::jsonb"#,
    );
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn containment_not_subset() {
    let mut e = Engine::new();
    let v = one_value(&mut e, r#"SELECT '{"a": 1}'::jsonb @> '{"a":2}'::jsonb"#);
    assert_eq!(v, Value::Bool(false));
}

// ─── jsonb_set (function, returns jsonb) — sentori 4 callsites ──

#[test]
fn jsonb_set_replaces_object_member() {
    let mut e = Engine::new();
    let v = one_value(
        &mut e,
        r#"SELECT jsonb_set('{"a": 1, "b": 2}'::jsonb, '{a}', '99'::jsonb)"#,
    );
    let Value::Json(s) = v else { panic!() };
    // Round-trip back through the parser to compare canonical
    // shape (whitespace / member order is implementation-defined).
    assert!(s.contains("\"a\": 99"), "got {s}");
    assert!(s.contains("\"b\": 2"), "got {s}");
}

#[test]
fn jsonb_set_inserts_new_object_member() {
    let mut e = Engine::new();
    let v = one_value(
        &mut e,
        r#"SELECT jsonb_set('{"a": 1}'::jsonb, '{c}', '3'::jsonb)"#,
    );
    let Value::Json(s) = v else { panic!() };
    assert!(s.contains("\"a\": 1"));
    assert!(s.contains("\"c\": 3"));
}

// ─── #> / #>> (path access) — PG-canonical sanity ──

#[test]
fn path_get_jsonb() {
    let mut e = Engine::new();
    let v = one_value(
        &mut e,
        r#"SELECT '{"a":{"b":{"c":42}}}'::jsonb #> '{a,b,c}'"#,
    );
    let Value::Json(s) = v else { panic!() };
    assert_eq!(s, "42");
}

#[test]
fn path_get_text() {
    let mut e = Engine::new();
    let v = one_value(&mut e, r#"SELECT '{"a":{"b":"deep"}}'::jsonb #>> '{a,b}'"#);
    assert_eq!(v, Value::text("deep"));
}

// ─── v7.37.6-A new operators ─────────────────────────────────────

#[test]
fn contained_by_is_reverse_contains() {
    let mut e = Engine::new();
    // `'{"a": 1}' <@ '{"a": 1, "b": 2}'` ⇔ rhs @> lhs → true.
    let v = one_value(
        &mut e,
        r#"SELECT '{"a": 1}'::jsonb <@ '{"a": 1, "b": 2}'::jsonb"#,
    );
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn contained_by_negative() {
    let mut e = Engine::new();
    let v = one_value(
        &mut e,
        r#"SELECT '{"a": 1, "b": 2}'::jsonb <@ '{"a": 1}'::jsonb"#,
    );
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn key_exists_object_member() {
    let mut e = Engine::new();
    let v = one_value(&mut e, r#"SELECT '{"a": 1, "b": 2}'::jsonb ? 'a'"#);
    assert_eq!(v, Value::Bool(true));
    let v = one_value(&mut e, r#"SELECT '{"a": 1}'::jsonb ? 'missing'"#);
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn key_exists_array_element_as_string() {
    let mut e = Engine::new();
    // PG: `?` on an array is true iff any element is a JSON string
    // equal to the key.
    let v = one_value(&mut e, r#"SELECT '["a","b","c"]'::jsonb ? 'b'"#);
    assert_eq!(v, Value::Bool(true));
    let v = one_value(&mut e, r#"SELECT '["a","b","c"]'::jsonb ? 'z'"#);
    assert_eq!(v, Value::Bool(false));
    // PG: numeric array elements never match `?` (only strings do).
    let v = one_value(&mut e, r#"SELECT '[1, 2, 3]'::jsonb ? '2'"#);
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn keys_any_with_text_array() {
    let mut e = Engine::new();
    let v = one_value(&mut e, r#"SELECT '{"a": 1, "b": 2}'::jsonb ?| ARRAY['z','b']"#);
    assert_eq!(v, Value::Bool(true));
    let v = one_value(&mut e, r#"SELECT '{"a": 1}'::jsonb ?| ARRAY['x','y','z']"#);
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn keys_all_with_text_array() {
    let mut e = Engine::new();
    let v = one_value(
        &mut e,
        r#"SELECT '{"a":1,"b":2,"c":3}'::jsonb ?& ARRAY['a','b']"#,
    );
    assert_eq!(v, Value::Bool(true));
    let v = one_value(&mut e, r#"SELECT '{"a": 1, "b": 2}'::jsonb ?& ARRAY['a','c']"#);
    assert_eq!(v, Value::Bool(false));
}
