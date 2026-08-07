//! PG `replace(string, from, to)` — substring substitution.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-string.html
//!   "Replaces all occurrences in string of substring from with
//!    substring to."
//!
//! Key invariants pinned here:
//!   * ALL occurrences replaced (not just first); non-overlapping
//!     greedy scan from left to right.
//!   * `from` empty string → input passed through unchanged
//!     (PG behavior; avoids infinite loop).
//!   * NULL on any arg → NULL.
//!   * Replacement happens in-place — `to` longer / shorter than
//!     `from` is fine. Overlapping replacements (when `to`
//!     contains `from`) do NOT recursively re-replace.
//!   * Multi-byte UTF-8 handled correctly.

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
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── BASIC ────────────────────────────────────────────────────────

#[test]
fn replace_single_occurrence() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT replace('hello', 'l', 'L')"), "heLLo");
}

#[test]
fn replace_all_occurrences_not_just_first() {
    // CRITICAL: PG replaces ALL occurrences, not just the first.
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT replace('aaa', 'a', 'b')"), "bbb");
}

#[test]
fn replace_substring_longer_than_one_char() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT replace('hello world', 'world', 'PG')"),
        "hello PG"
    );
}

#[test]
fn replace_with_longer_substring() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT replace('abc', 'b', 'BBB')"), "aBBBc");
}

#[test]
fn replace_with_shorter_substring() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT replace('aXXXb', 'XXX', 'Y')"), "aYb");
}

#[test]
fn replace_with_empty_to() {
    // Delete the from-substring entirely.
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT replace('aXbXc', 'X', '')"), "abc");
}

// ── EMPTY FROM EDGE CASE ─────────────────────────────────────────

#[test]
fn replace_empty_from_passes_through_unchanged() {
    // PG: empty `from` → string unchanged. Critical to avoid
    // infinite loop (without this guard, every position has
    // a match and the loop never terminates).
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT replace('hello', '', 'X')"), "hello");
}

// ── NO MATCH ─────────────────────────────────────────────────────

#[test]
fn replace_no_match_passes_through() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT replace('hello', 'XYZ', 'Q')"), "hello");
}

#[test]
fn replace_in_empty_input() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT replace('', 'a', 'b')"), "");
}

// ── NON-RECURSIVE / OVERLAP ──────────────────────────────────────

#[test]
fn replace_to_containing_from_does_not_recursively_replace() {
    // If we replace 'a' with 'aa', we should get 'aabb', NOT
    // infinite expansion. After replacing each original 'a',
    // we advance past the inserted 'aa'.
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT replace('abb', 'a', 'aa')"), "aabb");
}

#[test]
fn replace_overlapping_matches_left_to_right_greedy() {
    // PG: 'aaa' replace 'aa' with 'b' → 'ba' (first match consumes
    // first two chars; third char doesn't have a match).
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT replace('aaa', 'aa', 'b')"), "ba");
}

#[test]
fn replace_four_a_with_aa_target() {
    // 'aaaa' replace 'aa' with 'b' → 'bb' (two non-overlapping
    // matches).
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT replace('aaaa', 'aa', 'b')"), "bb");
}

// ── UNICODE / MULTI-BYTE ─────────────────────────────────────────

#[test]
fn replace_multibyte_from() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT replace('日本語日本語', '日本語', 'JP')"),
        "JPJP"
    );
}

#[test]
fn replace_multibyte_to() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT replace('hello', 'hello', '日本語')"),
        "日本語"
    );
}

#[test]
fn replace_ascii_in_unicode_context() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT replace('日a日a日', 'a', 'X')"),
        "日X日X日"
    );
}

// ── NULL HANDLING ────────────────────────────────────────────────

#[test]
fn replace_null_input_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT replace(NULL, 'a', 'b')").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn replace_null_from_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT replace('hello', NULL, 'b')").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn replace_null_to_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT replace('hello', 'l', NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

// ── ARITY ────────────────────────────────────────────────────────

#[test]
fn replace_too_few_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT replace('a', 'b')").is_err());
}

#[test]
fn replace_no_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT replace()").is_err());
}

#[test]
fn replace_too_many_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT replace('a', 'b', 'c', 'd')").is_err());
}

// ── TYPE COERCION ────────────────────────────────────────────────

#[test]
fn replace_numeric_args_coerced_to_text() {
    let mut e = Engine::new();
    // v7.39 (round 625) — PG has no `replace(integer, …)`.
    let m = e
        .execute("SELECT replace(12321, '2', 'X')")
        .expect_err("PG has no replace(integer, text, text)")
        .to_string();
    assert!(
        m.contains("function replace(integer, text, text) does not exist"),
        "{m}"
    );
}

// ── COLUMN-LEVEL ─────────────────────────────────────────────────

#[test]
fn replace_over_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (n TEXT NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES ('foo bar baz')").unwrap();
    let row = one_row(e.execute("SELECT replace(n, ' ', '-') FROM u").unwrap());
    assert_eq!(row[0], Value::text("foo-bar-baz"));
}

#[test]
fn replace_inside_where() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, p TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, '/users/alice'), (2, '/users/bob')")
        .unwrap();
    let r = e
        .execute("SELECT id FROM u WHERE replace(p, '/users/', '') = 'alice'")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(1));
}

#[test]
fn replace_inside_insert_values() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (n TEXT NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES (replace('a-b-c', '-', '_'))")
        .unwrap();
    let row = one_row(e.execute("SELECT n FROM u").unwrap());
    assert_eq!(row[0], Value::text("a_b_c"));
}

#[test]
fn nested_replace_calls() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT replace(replace('a-b_c', '-', '+'), '_', '=')"
        ),
        "a+b=c"
    );
}

// ── TYPE / METADATA ──────────────────────────────────────────────

#[test]
fn replace_column_type_is_text() {
    let mut e = Engine::new();
    let r = e.execute("SELECT replace('a', 'a', 'b')").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, spg_storage::DataType::Text);
}
