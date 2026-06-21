//! PG `lpad(string, length [, fill])` / `rpad(string, length [, fill])`.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-string.html
//!
//! Invariants pinned:
//!   * `length` is the TARGET number of *characters* (not bytes).
//!   * If `length(string) > target`, the input is TRUNCATED to
//!     `target` chars (lpad keeps right side; rpad keeps left).
//!   * If `length(string) < target`, the input is padded with
//!     `fill` (cycling if fill is multi-char). Default `fill` is
//!     a single SPACE.
//!   * `length` <= 0 → '' (PG verified).
//!   * `fill` empty + needs padding → just truncates / keeps the
//!     input verbatim (PG behavior — can't pad with nothing).
//!   * NULL on ANY arg → NULL.
//!   * Multi-byte chars in input AND in fill are codepoint-counted.

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

// ── lpad ─────────────────────────────────────────────────────────

#[test]
fn lpad_pad_zeroes_for_id() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT lpad('5', 3, '0')"), "005");
}

#[test]
fn lpad_default_space_fill() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT lpad('5', 3)"), "  5");
}

#[test]
fn lpad_input_already_long_enough_is_truncated_right_aligned() {
    // PG verified: lpad('helloXX', 5) → 'hello' (truncated from
    // right; left side kept).
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT lpad('helloXX', 5)"), "hello");
}

#[test]
fn lpad_input_equal_length_passthrough() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT lpad('hello', 5, 'X')"), "hello");
}

#[test]
fn lpad_length_zero_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT lpad('hello', 0, 'X')"), "");
}

#[test]
fn lpad_negative_length_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT lpad('hello', -1, 'X')"), "");
}

#[test]
fn lpad_multi_char_fill_cycles() {
    // PG: pad cycles. lpad('a', 7, 'xy') → 'xyxyxya'.
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT lpad('a', 7, 'xy')"), "xyxyxya");
}

#[test]
fn lpad_multi_char_fill_partial_cycle() {
    let mut e = Engine::new();
    // Need 5 padding chars, fill 'xyz' (3) → cycle gives 'xyzxy'.
    assert_eq!(text(&mut e, "SELECT lpad('a', 6, 'xyz')"), "xyzxya");
}

#[test]
fn lpad_empty_fill_passes_input_through_no_pad() {
    // PG: empty fill — can't pad, returns input verbatim
    // (truncated if too long).
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT lpad('hello', 10, '')"), "hello");
    assert_eq!(text(&mut e, "SELECT lpad('helloextra', 5, '')"), "hello");
}

// ── rpad ─────────────────────────────────────────────────────────

#[test]
fn rpad_basic() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT rpad('5', 3, '0')"), "500");
}

#[test]
fn rpad_default_space() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT rpad('5', 3)"), "5  ");
}

#[test]
fn rpad_truncates_from_right_left_aligned() {
    // PG: rpad('helloXX', 5) → 'hello' (left side kept).
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT rpad('helloXX', 5)"), "hello");
}

#[test]
fn rpad_multi_char_fill_cycles() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT rpad('a', 7, 'xy')"), "axyxyxy");
}

#[test]
fn rpad_length_zero_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT rpad('hello', 0, 'X')"), "");
}

// ── UNICODE ──────────────────────────────────────────────────────

#[test]
fn lpad_input_multibyte_codepoint_count() {
    // Length is CHAR count, not bytes. '日本' = 2 chars.
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT lpad('日本', 5, '_')"), "___日本");
}

#[test]
fn lpad_fill_multibyte() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT lpad('a', 4, '日')"), "日日日a");
}

#[test]
fn rpad_input_multibyte_codepoint_count() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT rpad('日本', 5, '_')"), "日本___");
}

#[test]
fn lpad_truncate_multibyte_from_right() {
    let mut e = Engine::new();
    // '日本語' = 3 chars. lpad(..., 2) → truncate keeping first 2.
    assert_eq!(text(&mut e, "SELECT lpad('日本語', 2)"), "日本");
}

#[test]
fn rpad_truncate_multibyte_from_right() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT rpad('日本語', 2)"), "日本");
}

// ── NULL HANDLING ────────────────────────────────────────────────

#[test]
fn lpad_null_input_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT lpad(NULL, 5, 'x')").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn lpad_null_length_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT lpad('a', NULL, 'x')").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn lpad_null_fill_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT lpad('a', 5, NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn rpad_null_input_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT rpad(NULL, 5)").unwrap());
    assert_eq!(row[0], Value::Null);
}

// ── ARITY ────────────────────────────────────────────────────────

#[test]
fn lpad_one_arg_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT lpad('a')").is_err());
}

#[test]
fn lpad_too_many_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT lpad('a', 1, 'x', 'y')").is_err());
}

// ── INSIDE WHERE / INSERT ────────────────────────────────────────

#[test]
fn lpad_zero_padded_ids_for_display() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE invoices (n INT NOT NULL)").unwrap();
    e.execute("INSERT INTO invoices VALUES (1), (42), (1234)")
        .unwrap();
    let r = e
        .execute("SELECT lpad(n::TEXT, 6, '0') FROM invoices ORDER BY n")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows[0].values[0], Value::text("000001"));
    assert_eq!(rows[1].values[0], Value::text("000042"));
    assert_eq!(rows[2].values[0], Value::text("001234"));
}

#[test]
fn rpad_inside_insert_values() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (name_field TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (rpad('Alice', 10, '.'))")
        .unwrap();
    let row = one_row(e.execute("SELECT name_field FROM u").unwrap());
    assert_eq!(row[0], Value::text("Alice....."));
}

// ── COLUMN TYPE ──────────────────────────────────────────────────

#[test]
fn lpad_column_type_is_text() {
    let mut e = Engine::new();
    let r = e.execute("SELECT lpad('a', 3, 'x')").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, spg_storage::DataType::Text);
}
