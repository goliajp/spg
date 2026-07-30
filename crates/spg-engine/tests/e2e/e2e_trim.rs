//! PG `trim(s)` / `trim(s, chars)` family.
//!
//! Surface in this commit (function-call form):
//!   * `trim(s)`         — strip default char (SPACE) both ends
//!   * `trim(s, chars)`  — strip given char set both ends
//!   * `ltrim(s)` / `ltrim(s, chars)` — left only
//!   * `rtrim(s)` / `rtrim(s, chars)` — right only
//!   * `btrim(s)` / `btrim(s, chars)` — PG alias of trim
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-string.html
//!
//! Default char set is the single ASCII SPACE (PG-canonical),
//! NOT the broader "whitespace" set. Customers expecting tab /
//! newline removal pass them explicitly.

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

fn text(e: &mut Engine, sql: &str) -> String {
    let r = e.execute(sql).unwrap_or_else(|err| {
        panic!("execute({sql:?}) failed: {err:?}");
    });
    let row = one_row(r);
    match &row[0] {
        Value::Text(s) => s.to_string(),
        Value::Null => panic!("got NULL, expected Text"),
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── trim() default-space behavior ────────────────────────────────

#[test]
fn trim_strips_leading_and_trailing_space() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT trim('  hello  ')"), "hello");
}

#[test]
fn trim_only_leading_space() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT trim('  hello')"), "hello");
}

#[test]
fn trim_only_trailing_space() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT trim('hello  ')"), "hello");
}

#[test]
fn trim_inner_space_preserved() {
    // CRITICAL: only end whitespace stripped, not inner.
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT trim('  hello world  ')"),
        "hello world"
    );
}

#[test]
fn trim_all_space_yields_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT trim('     ')"), "");
}

#[test]
fn trim_empty_input_yields_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT trim('')"), "");
}

#[test]
fn trim_no_strippable_chars_passthrough() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT trim('hello')"), "hello");
}

#[test]
fn trim_does_not_strip_tab_or_newline_by_default() {
    // PG verified: default char set is SPACE only, not the
    // POSIX whitespace class.
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT trim('\thello\n')"), "\thello\n");
}

// ── trim() with explicit chars ───────────────────────────────────

#[test]
fn trim_with_chars_strips_given_set() {
    let mut e = Engine::new();
    // PG: trim(text, characters) — note PG's positional order is
    // `trim(characters FROM string)` in standard form, but the
    // function-call form here is `btrim(string, chars)`. We
    // wire trim(s, chars) to the same shape.
    assert_eq!(text(&mut e, "SELECT trim('xxhelloxx', 'x')"), "hello");
}

#[test]
fn trim_with_multi_char_set() {
    let mut e = Engine::new();
    // chars is treated as a SET — any char in the set is stripped.
    assert_eq!(text(&mut e, "SELECT trim('xyhelloyx', 'xy')"), "hello");
}

#[test]
fn trim_with_chars_does_not_strip_inner_occurrences() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT trim('xhelloxxworld', 'x')"),
        "helloxxworld"
    );
}

#[test]
fn trim_chars_unicode_multibyte_set() {
    // PG treats characters as a UTF-8 codepoint set, not bytes.
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT trim('日hello日', '日')"), "hello");
}

// ── ltrim / rtrim ────────────────────────────────────────────────

#[test]
fn ltrim_strips_left_only_default_space() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT ltrim('  hello  ')"), "hello  ");
}

#[test]
fn rtrim_strips_right_only_default_space() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT rtrim('  hello  ')"), "  hello");
}

#[test]
fn ltrim_with_chars() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT ltrim('xxhello', 'x')"), "hello");
}

#[test]
fn rtrim_with_chars() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT rtrim('helloxx', 'x')"), "hello");
}

#[test]
fn ltrim_doesnt_touch_right_side() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT ltrim('xxhelloxx', 'x')"), "helloxx");
}

#[test]
fn rtrim_doesnt_touch_left_side() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT rtrim('xxhelloxx', 'x')"), "xxhello");
}

#[test]
fn ltrim_empty_input() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT ltrim('')"), "");
}

#[test]
fn rtrim_empty_input() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT rtrim('')"), "");
}

// ── btrim alias ──────────────────────────────────────────────────

#[test]
fn btrim_is_same_as_trim() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT btrim('  hello  ')"), "hello");
    assert_eq!(text(&mut e, "SELECT btrim('xxhelloxx', 'x')"), "hello");
}

// ── NULL HANDLING ────────────────────────────────────────────────

#[test]
fn trim_null_input_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT trim(NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn trim_null_chars_returns_null() {
    // PG: trim(s, NULL) → NULL (the chars arg is part of the
    // computation; NULL poisons).
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT trim('xx', NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn ltrim_null_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT ltrim(NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn rtrim_null_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT rtrim(NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

// ── TYPE / METADATA ──────────────────────────────────────────────

#[test]
fn trim_column_type_is_text() {
    let mut e = Engine::new();
    let r = e.execute("SELECT trim('  x  ')").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, spg_storage::DataType::Text);
}

// ── COLUMN REFS ──────────────────────────────────────────────────

#[test]
fn trim_over_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (n TEXT NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES ('   john   '), ('   ')")
        .unwrap();
    let r = e.execute("SELECT trim(n) FROM u ORDER BY n").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows[0].values[0], Value::text(String::new()));
    assert_eq!(rows[1].values[0], Value::text("john"));
}

// ── ARITY ERRORS ─────────────────────────────────────────────────

#[test]
fn trim_no_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT trim()").is_err());
}

#[test]
fn trim_too_many_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT trim('a', 'b', 'c')").is_err());
}

#[test]
fn ltrim_no_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT ltrim()").is_err());
}

// ── EDGE: NUMERIC INPUT (PG: coerced) ────────────────────────────

#[test]
fn trim_numeric_input_coerced_to_text() {
    let mut e = Engine::new();
    // v7.39 (round 625) — the comment above claimed "PG: trim(42) -> '42'".
    // PG does not: it resolves trim to pg_catalog.btrim and says the
    // function does not exist. The claim was never checked.
    let m = e
        .execute("SELECT trim(42)")
        .expect_err("PG rejects trim(integer)")
        .to_string();
    assert!(m.contains("function pg_catalog.btrim(integer) does not exist"), "{m}");
}

// ── INSIDE WHERE / INSERT ────────────────────────────────────────

#[test]
fn trim_inside_where() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, n TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, ' alice '), (2, ' bob ')")
        .unwrap();
    let r = e
        .execute("SELECT id FROM u WHERE trim(n) = 'alice'")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(1));
}

#[test]
fn trim_inside_insert_values() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (n TEXT NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES (trim('   hello   '))")
        .unwrap();
    let row = one_row(e.execute("SELECT n FROM u").unwrap());
    assert_eq!(row[0], Value::text("hello"));
}

// ── NESTED ───────────────────────────────────────────────────────

#[test]
fn nested_trim_calls() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT trim(trim('   hello   '), 'h')"),
        "ello"
    );
}
