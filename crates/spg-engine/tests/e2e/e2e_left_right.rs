//! PG `left(string, n)` / `right(string, n)` — head/tail
//! substring helpers.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-string.html
//!   "left(string text, n integer) — Returns first n characters
//!    in the string. When n is negative, returns all but last
//!    |n| characters."
//!   "right(string text, n integer) — Returns last n characters
//!    in the string. When n is negative, returns all but first
//!    |n| characters."
//!
//! Invariants pinned:
//!   * `left(s, n)` n>=0 → first n chars (or whole string if
//!     n >= length).
//!   * `left(s, n)` n<0 → all-but-last |n| chars (slice cut from
//!     the right side, |n| chars dropped).
//!   * `right(s, n)` n>=0 → last n chars.
//!   * `right(s, n)` n<0 → all-but-first |n| chars.
//!   * n=0 → ''.
//!   * |n| >= length → '' for the negative-symmetric case
//!     (left negative or right negative consuming everything).
//!   * Codepoint-counted, NOT byte-counted.
//!   * NULL on any arg → NULL.
//!   * `left` is a reserved-keyword token in the SQL grammar
//!     (LEFT [OUTER] JOIN); the parser must accept it as a
//!     function-call ident when followed by `(`.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one_row(r: QueryResult) -> Vec<Value<'static>> {
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            rows.into_iter().next().unwrap().values
        }
        _ => panic!(),
    }
}

fn text(e: &mut Engine, sql: &str) -> String {
    let row = one_row(
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}")),
    );
    match &row[0] {
        Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── left() — basic ───────────────────────────────────────────────

#[test]
fn left_basic() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT left('hello', 3)"), "hel");
}

#[test]
fn left_n_equals_length_returns_whole() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT left('hello', 5)"), "hello");
}

#[test]
fn left_n_greater_than_length_returns_whole() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT left('hello', 10)"), "hello");
}

#[test]
fn left_n_zero_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT left('hello', 0)"), "");
}

// ── left() — negative n (slice from the right) ────────────────────

#[test]
fn left_negative_n_drops_last_n_chars() {
    // PG: left('hello', -2) → 'hel' (drop last 2)
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT left('hello', -2)"), "hel");
}

#[test]
fn left_negative_n_equals_length_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT left('hello', -5)"), "");
}

#[test]
fn left_negative_n_beyond_length_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT left('hello', -100)"), "");
}

// ── right() — basic ──────────────────────────────────────────────

#[test]
fn right_basic() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT right('hello', 3)"), "llo");
}

#[test]
fn right_n_equals_length_returns_whole() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT right('hello', 5)"), "hello");
}

#[test]
fn right_n_greater_than_length_returns_whole() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT right('hello', 10)"), "hello");
}

#[test]
fn right_n_zero_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT right('hello', 0)"), "");
}

// ── right() — negative n (slice from the left) ───────────────────

#[test]
fn right_negative_n_drops_first_n_chars() {
    // PG: right('hello', -2) → 'llo' (drop first 2)
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT right('hello', -2)"), "llo");
}

#[test]
fn right_negative_n_equals_length_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT right('hello', -5)"), "");
}

#[test]
fn right_negative_n_beyond_length_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT right('hello', -100)"), "");
}

// ── EMPTY INPUT ──────────────────────────────────────────────────

#[test]
fn left_empty_input_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT left('', 3)"), "");
}

#[test]
fn right_empty_input_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT right('', 3)"), "");
}

// ── UNICODE ──────────────────────────────────────────────────────

#[test]
fn left_unicode_codepoint_counted() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT left('日本語', 2)"), "日本");
}

#[test]
fn right_unicode_codepoint_counted() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT right('日本語', 2)"), "本語");
}

#[test]
fn left_unicode_negative_n() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT left('日本語', -1)"), "日本");
}

// ── NULL HANDLING ────────────────────────────────────────────────

#[test]
fn left_null_string_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT left(NULL, 3)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn left_null_n_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT left('a', NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn right_null_string_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT right(NULL, 3)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn right_null_n_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT right('a', NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

// ── ARITY / TYPE ─────────────────────────────────────────────────

#[test]
fn left_one_arg_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT left('a')").is_err());
}

#[test]
fn left_three_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT left('a', 1, 2)").is_err());
}

#[test]
fn left_n_non_integer_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT left('a', 'foo')").is_err());
}

#[test]
fn left_numeric_input_coerced() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT left(12345, 3)"), "123");
}

// ── INTEGRATION ─────────────────────────────────────────────────

#[test]
fn left_inside_where() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, code TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 'US-12345'), (2, 'EU-67890')")
        .unwrap();
    let r = e
        .execute("SELECT id FROM u WHERE left(code, 2) = 'US'")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(1));
}

#[test]
fn right_inside_insert_values() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (last3 TEXT NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES (right('abc-XYZ', 3))")
        .unwrap();
    let row = one_row(e.execute("SELECT last3 FROM u").unwrap());
    assert_eq!(row[0], Value::text("XYZ"));
}

// ── COLUMN TYPE ──────────────────────────────────────────────────

#[test]
fn left_column_type_is_text() {
    let mut e = Engine::new();
    let r = e.execute("SELECT left('a', 1)").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, spg_storage::DataType::Text);
}

// ── PARSER: Token::Left handled ──────────────────────────────────

#[test]
fn left_does_not_collide_with_left_join_keyword() {
    // Regression for the Token::Left reserved-keyword issue —
    // `left` followed by `(` must be parsed as a function call,
    // NOT as the start of a `LEFT OUTER JOIN` clause.
    let mut e = Engine::new();
    e.execute("CREATE TABLE a (id INT NOT NULL)").unwrap();
    e.execute("CREATE TABLE b (id INT NOT NULL, label TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO a VALUES (1)").unwrap();
    e.execute("INSERT INTO b VALUES (1, 'hello')").unwrap();
    // Mix LEFT JOIN with a left() function call in the SELECT.
    let r = e
        .execute(
            "SELECT a.id, left(b.label, 3) \
             FROM a LEFT JOIN b ON a.id = b.id",
        )
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[1], Value::text("hel"));
}
