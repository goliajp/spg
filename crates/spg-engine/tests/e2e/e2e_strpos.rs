//! PG `strpos(string, substring)` — substring position lookup.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-string.html
//!   "Returns the location of the specified substring, or 0 if
//!    it's not present."
//!
//! Arg order is `strpos(haystack, needle)` — opposite to PG's
//! `position(needle IN haystack)` standard form. This is the
//! function-call shape; both are wired.
//!
//! Invariants pinned:
//!   * 1-indexed (PG verified). 0 = not found.
//!   * Codepoint-counted, NOT byte-counted.
//!   * Empty substring → 1 (PG verified).
//!   * Empty haystack + non-empty needle → 0.
//!   * Empty haystack + empty needle → 1.
//!   * Multi-byte UTF-8 handled correctly.
//!   * NULL on any arg → NULL.

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

fn int(e: &mut Engine, sql: &str) -> i32 {
    let row = one_row(
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}")),
    );
    match &row[0] {
        Value::Int(n) => *n,
        Value::BigInt(n) => *n as i32,
        other => panic!("expected Int, got {other:?}"),
    }
}

// ── BASIC ────────────────────────────────────────────────────────

#[test]
fn strpos_found_at_start() {
    let mut e = Engine::new();
    assert_eq!(int(&mut e, "SELECT strpos('hello', 'he')"), 1);
}

#[test]
fn strpos_found_in_middle() {
    let mut e = Engine::new();
    assert_eq!(int(&mut e, "SELECT strpos('hello', 'll')"), 3);
}

#[test]
fn strpos_found_at_end() {
    let mut e = Engine::new();
    assert_eq!(int(&mut e, "SELECT strpos('hello', 'o')"), 5);
}

#[test]
fn strpos_returns_first_match_only() {
    // 'hello' contains 'l' at positions 3 and 4; first wins.
    let mut e = Engine::new();
    assert_eq!(int(&mut e, "SELECT strpos('hello', 'l')"), 3);
}

#[test]
fn strpos_not_found_returns_zero() {
    let mut e = Engine::new();
    assert_eq!(int(&mut e, "SELECT strpos('hello', 'XYZ')"), 0);
}

#[test]
fn strpos_needle_longer_than_haystack_returns_zero() {
    let mut e = Engine::new();
    assert_eq!(int(&mut e, "SELECT strpos('hi', 'hello')"), 0);
}

// ── EMPTY STRING CASES (PG-canonical) ────────────────────────────

#[test]
fn strpos_empty_needle_returns_one() {
    // PG: strpos('hello', '') → 1
    let mut e = Engine::new();
    assert_eq!(int(&mut e, "SELECT strpos('hello', '')"), 1);
}

#[test]
fn strpos_empty_haystack_nonempty_needle_returns_zero() {
    let mut e = Engine::new();
    assert_eq!(int(&mut e, "SELECT strpos('', 'a')"), 0);
}

#[test]
fn strpos_both_empty_returns_one() {
    let mut e = Engine::new();
    assert_eq!(int(&mut e, "SELECT strpos('', '')"), 1);
}

// ── UNICODE / CODEPOINT-COUNTED ──────────────────────────────────

#[test]
fn strpos_unicode_haystack_codepoint_position() {
    // '日本語' chars at positions 1 / 2 / 3 (NOT byte positions).
    let mut e = Engine::new();
    assert_eq!(int(&mut e, "SELECT strpos('日本語', '本')"), 2);
}

#[test]
fn strpos_unicode_at_end() {
    let mut e = Engine::new();
    assert_eq!(int(&mut e, "SELECT strpos('日本語', '語')"), 3);
}

#[test]
fn strpos_unicode_substring() {
    let mut e = Engine::new();
    assert_eq!(int(&mut e, "SELECT strpos('日本語', '本語')"), 2);
}

#[test]
fn strpos_unicode_not_found() {
    let mut e = Engine::new();
    assert_eq!(int(&mut e, "SELECT strpos('日本語', '中')"), 0);
}

#[test]
fn strpos_ascii_in_unicode_haystack() {
    // Position should still be codepoint-counted.
    let mut e = Engine::new();
    assert_eq!(int(&mut e, "SELECT strpos('日aXa日', 'X')"), 3);
}

// ── CASE-SENSITIVITY ─────────────────────────────────────────────

#[test]
fn strpos_is_case_sensitive() {
    // PG: strpos is case-sensitive. 'HELLO' has no 'l'.
    let mut e = Engine::new();
    assert_eq!(int(&mut e, "SELECT strpos('HELLO', 'l')"), 0);
}

// ── NULL HANDLING ────────────────────────────────────────────────

#[test]
fn strpos_null_haystack_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT strpos(NULL, 'a')").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn strpos_null_needle_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT strpos('a', NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

// ── ARITY ────────────────────────────────────────────────────────

#[test]
fn strpos_one_arg_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT strpos('a')").is_err());
}

#[test]
fn strpos_three_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT strpos('a', 'b', 'c')").is_err());
}

// ── ARG ORDER DIFFERS FROM position() ────────────────────────────

#[test]
fn strpos_arg_order_differs_from_position() {
    // PG: position('needle' IN 'haystack') uses opposite order
    // to strpos('haystack', 'needle'). They should agree on
    // result but disagree on arg position.
    let mut e = Engine::new();
    // strpos('hello', 'll') → 3
    assert_eq!(int(&mut e, "SELECT strpos('hello', 'll')"), 3);
    // PG position() function-form: position(needle, haystack)
    assert_eq!(int(&mut e, "SELECT position('ll', 'hello')"), 3);
}

// ── TYPE COERCION ────────────────────────────────────────────────

#[test]
fn strpos_numeric_args_coerced() {
    let mut e = Engine::new();
    // strpos('12345', '3') → position 3.
    // v7.39 (round 625) — PG has no `strpos(integer, …)`.
    let m = e
        .execute("SELECT strpos(12345, '3')")
        .expect_err("PG has no strpos(integer, text)")
        .to_string();
    assert!(
        m.contains("function strpos(integer, text) does not exist"),
        "{m}"
    );
}

// ── COLUMN / WHERE / INSERT ──────────────────────────────────────

#[test]
fn strpos_inside_where() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, email TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 'alice@example.com'), (2, 'no-at-here')")
        .unwrap();
    let r = e
        .execute("SELECT id FROM u WHERE strpos(email, '@') > 0")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(1));
}

#[test]
fn strpos_inside_insert_values() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (pos INT NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES (strpos('hello', 'll'))")
        .unwrap();
    let row = one_row(e.execute("SELECT pos FROM u").unwrap());
    assert_eq!(row[0], Value::Int(3));
}

// ── COLUMN TYPE ──────────────────────────────────────────────────

#[test]
fn strpos_column_type_is_int() {
    let mut e = Engine::new();
    let r = e.execute("SELECT strpos('a', 'a')").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, spg_storage::DataType::Int);
}
