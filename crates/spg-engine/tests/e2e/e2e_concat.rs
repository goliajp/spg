//! PG `concat(args...)` — variadic; coerces every arg to text;
//! NULL arguments silently skipped (PG semantics, NOT MySQL).
//!
//! Reference: <https://www.postgresql.org/docs/current/functions-string.html>
//! > `concat(val1 "any" [, val2 "any" [, ...] ])` — Concatenates
//! > the text representations of all the arguments. NULL
//! > arguments are ignored.
//!
//! TDD test surface for v7.17+ infrastructure-grade function add.
//! Every edge case covered before the implementation lands.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one_row(r: QueryResult) -> Vec<Value<'static>> {
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "expected exactly 1 row");
            rows.into_iter().next().unwrap().values
        }
        _ => panic!("expected rows"),
    }
}

fn text_result(e: &mut Engine, sql: &str) -> String {
    let r = e.execute(sql).unwrap_or_else(|err| {
        panic!("execute({sql:?}) failed: {err:?}");
    });
    let row = one_row(r);
    match &row[0] {
        Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── ARITY ─────────────────────────────────────────────────────────

#[test]
fn zero_args_returns_empty_string() {
    // PG verified: `SELECT concat()` → `''`
    let mut e = Engine::new();
    assert_eq!(text_result(&mut e, "SELECT concat()"), "");
}

#[test]
fn single_arg_returns_text_form() {
    let mut e = Engine::new();
    assert_eq!(text_result(&mut e, "SELECT concat('hello')"), "hello");
}

#[test]
fn two_args_concatenated() {
    let mut e = Engine::new();
    assert_eq!(text_result(&mut e, "SELECT concat('foo', 'bar')"), "foobar");
}

#[test]
fn five_args_concatenated_in_order() {
    let mut e = Engine::new();
    assert_eq!(
        text_result(&mut e, "SELECT concat('a','b','c','d','e')"),
        "abcde"
    );
}

#[test]
fn many_args_stress() {
    // 100 single-char args.
    let mut e = Engine::new();
    let args: Vec<String> = (0..100).map(|i| format!("'{}'", i % 10)).collect();
    let sql = format!("SELECT concat({})", args.join(","));
    let r = text_result(&mut e, &sql);
    assert_eq!(r.len(), 100);
    // First 10 chars should be 0..9 followed by repeat.
    assert!(r.starts_with("0123456789"));
}

// ── NULL HANDLING (PG: SKIP) ──────────────────────────────────────

#[test]
fn null_arg_skipped() {
    // PG: concat('a', NULL, 'b') → 'ab'
    let mut e = Engine::new();
    assert_eq!(text_result(&mut e, "SELECT concat('a', NULL, 'b')"), "ab");
}

#[test]
fn all_null_args_returns_empty_string() {
    // PG verified: every NULL → '' (not NULL).
    let mut e = Engine::new();
    assert_eq!(text_result(&mut e, "SELECT concat(NULL, NULL)"), "");
}

#[test]
fn single_null_arg_returns_empty_string() {
    let mut e = Engine::new();
    assert_eq!(text_result(&mut e, "SELECT concat(NULL)"), "");
}

#[test]
fn nulls_between_text_skipped() {
    let mut e = Engine::new();
    assert_eq!(
        text_result(&mut e, "SELECT concat(NULL, 'x', NULL, 'y', NULL, 'z')"),
        "xyz"
    );
}

// ── EMPTY-STRING vs NULL ──────────────────────────────────────────

#[test]
fn empty_string_arg_included() {
    // Empty string is NOT NULL — it should appear (as zero chars).
    let mut e = Engine::new();
    assert_eq!(text_result(&mut e, "SELECT concat('a', '', 'b')"), "ab");
}

#[test]
fn only_empty_strings_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text_result(&mut e, "SELECT concat('', '', '')"), "");
}

// ── TYPE COERCION ────────────────────────────────────────────────

#[test]
fn integer_arg_coerced_to_text() {
    let mut e = Engine::new();
    assert_eq!(text_result(&mut e, "SELECT concat('id=', 42)"), "id=42");
}

#[test]
fn negative_integer_kept_negative() {
    let mut e = Engine::new();
    assert_eq!(text_result(&mut e, "SELECT concat('=', -7)"), "=-7");
}

#[test]
fn bigint_arg_coerced() {
    let mut e = Engine::new();
    assert_eq!(
        text_result(&mut e, "SELECT concat('big=', 9223372036854775807)"),
        "big=9223372036854775807"
    );
}

#[test]
fn smallint_arg_coerced() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (s SMALLINT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (32767)").unwrap();
    assert_eq!(
        text_result(&mut e, "SELECT concat('s=', s) FROM t"),
        "s=32767"
    );
}

#[test]
fn float_arg_coerced_to_text() {
    let mut e = Engine::new();
    let r = text_result(&mut e, "SELECT concat('x=', 1.5)");
    assert!(r.starts_with("x=1.5"), "got {r:?}");
}

#[test]
fn bool_true_renders_as_pg_letter_t() {
    // PG: concat(true) → 't'
    let mut e = Engine::new();
    assert_eq!(text_result(&mut e, "SELECT concat(true)"), "t");
}

#[test]
fn bool_false_renders_as_pg_letter_f() {
    let mut e = Engine::new();
    assert_eq!(text_result(&mut e, "SELECT concat(false)"), "f");
}

#[test]
fn mixed_types_all_coerced() {
    let mut e = Engine::new();
    assert_eq!(
        text_result(&mut e, "SELECT concat('a', 1, 'b', true, 'c', NULL)"),
        "a1btc"
    );
}

// ── UNICODE / MULTI-BYTE ──────────────────────────────────────────

#[test]
fn multibyte_utf8_preserved() {
    let mut e = Engine::new();
    assert_eq!(
        text_result(&mut e, "SELECT concat('日本語', '——', 'OK')"),
        "日本語——OK"
    );
}

#[test]
fn emoji_preserved() {
    let mut e = Engine::new();
    assert_eq!(
        text_result(&mut e, "SELECT concat('hi ', '👋', '!')"),
        "hi 👋!"
    );
}

#[test]
fn ascii_control_chars_preserved() {
    let mut e = Engine::new();
    // Tab character literal — PG accepts it inside a string literal.
    let r = e.execute("SELECT concat('a', '\t', 'b')").expect("execute");
    let row = one_row(r);
    let Value::Text(s) = &row[0] else { panic!() };
    assert_eq!(s, "a\tb");
}

// ── INTERACTIONS WITH COLUMNS ─────────────────────────────────────

#[test]
fn concat_on_text_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (first_name TEXT NOT NULL, last_name TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES ('John', 'Doe')").unwrap();
    let r = e
        .execute("SELECT concat(first_name, ' ', last_name) FROM u")
        .unwrap();
    let row = one_row(r);
    assert_eq!(row[0], Value::text("John Doe"));
}

#[test]
fn concat_with_nullable_column_skips_null_row() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (a TEXT, b TEXT)").unwrap();
    e.execute("INSERT INTO u VALUES ('x', NULL)").unwrap();
    let r = e.execute("SELECT concat(a, b) FROM u").unwrap();
    let row = one_row(r);
    // NULL b is skipped — result is 'x'.
    assert_eq!(row[0], Value::text("x"));
}

#[test]
fn concat_returns_non_null_even_when_every_input_null() {
    // Critical PG semantic: NEVER returns NULL.
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (a TEXT, b TEXT)").unwrap();
    e.execute("INSERT INTO u VALUES (NULL, NULL)").unwrap();
    let r = e.execute("SELECT concat(a, b) FROM u").unwrap();
    let row = one_row(r);
    assert_eq!(row[0], Value::text(String::new()));
    assert!(!matches!(row[0], Value::Null));
}

// ── DIFFERENTIATION FROM `||` ─────────────────────────────────────

#[test]
fn pipe_pipe_returns_null_on_null_arg() {
    // Reference behavior — `||` (text concat operator) IS
    // NULL-sensitive. This test exists so concat()'s
    // NULL-skip semantic is verified to differ.
    let mut e = Engine::new();
    let r = e.execute("SELECT 'a' || NULL || 'b'").unwrap();
    let row = one_row(r);
    assert_eq!(row[0], Value::Null);
}

// ── RESULT TYPE ──────────────────────────────────────────────────

#[test]
fn return_type_is_text_not_varchar() {
    let mut e = Engine::new();
    let r = e.execute("SELECT concat('a', 'b')").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, spg_storage::DataType::Text);
}

// ── INTEGRATION ───────────────────────────────────────────────────

#[test]
fn concat_inside_where_clause() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, n TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 'a'), (2, 'b')")
        .unwrap();
    let r = e
        .execute("SELECT id FROM u WHERE concat(n, '!') = 'b!'")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(2));
}

#[test]
fn concat_used_in_order_by() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (a TEXT NOT NULL, b TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES ('z', 'a'), ('a', 'z')")
        .unwrap();
    let r = e
        .execute("SELECT a, b FROM u ORDER BY concat(a, b)")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    // concat results: 'za', 'az'. Sorted: 'az' first then 'za'.
    assert_eq!(rows[0].values[0], Value::text("a"));
}

#[test]
fn concat_inside_insert_value() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (display TEXT NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES (concat('U-', 42))")
        .unwrap();
    let r = e.execute("SELECT display FROM u").unwrap();
    let row = one_row(r);
    assert_eq!(row[0], Value::text("U-42"));
}

#[test]
fn nested_concat_calls() {
    let mut e = Engine::new();
    assert_eq!(
        text_result(&mut e, "SELECT concat(concat('A', 'B'), concat('C', 'D'))"),
        "ABCD"
    );
}
