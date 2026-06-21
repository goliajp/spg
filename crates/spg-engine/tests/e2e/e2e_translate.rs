//! PG `translate(s, from, to)` — char-by-char mapping.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-string.html
//!   "Replaces each character in string that matches a character
//!    in the from set with the corresponding character in the to
//!    set. If from is longer than to, occurrences of the extra
//!    characters in from are removed."
//!
//! Invariants pinned:
//!   * Char-by-char: NOT substring substitution (distinct from replace).
//!   * `from`/`to` are positional codepoint maps.
//!   * Extra chars in `from` (beyond `to`'s length) → deleted.
//!   * Chars not in `from` → passed through verbatim.
//!   * NULL on any arg → NULL.
//!   * Multi-byte UTF-8 handled per-codepoint.

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

#[test]
fn translate_simple_char_swap() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT translate('hello', 'l', 'L')"), "heLLo");
}

#[test]
fn translate_positional_mapping() {
    // 'a' → 'X', 'b' → 'Y', 'c' → 'Z'.
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT translate('aabbcc', 'abc', 'XYZ')"),
        "XXYYZZ"
    );
}

#[test]
fn translate_chars_not_in_from_pass_through() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT translate('hello world', 'lo', 'L0')"),
        "heLL0 w0rLd"
    );
}

#[test]
fn translate_extra_from_chars_deleted() {
    // 'from'='abc' (3), 'to'='X' (1) → 'a' → 'X', 'b' & 'c' deleted.
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT translate('abcdef', 'abc', 'X')"),
        "Xdef"
    );
}

#[test]
fn translate_empty_to_deletes_all_from_chars() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT translate('hello world', 'lo', '')"),
        "he wrd"
    );
}

#[test]
fn translate_empty_from_passes_through_unchanged() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT translate('hello', '', 'X')"), "hello");
}

#[test]
fn translate_empty_input() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT translate('', 'a', 'b')"), "");
}

#[test]
fn translate_duplicate_chars_in_from_first_match_wins() {
    // PG: when `from` has duplicates, the FIRST occurrence's
    // mapping wins.
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT translate('aaa', 'aaa', 'XYZ')"), "XXX");
}

#[test]
fn translate_multibyte_input() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT translate('日本aaa', 'a', '*')"),
        "日本***"
    );
}

#[test]
fn translate_multibyte_in_from_set() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT translate('日本語', '日本', 'JP')"),
        "JP語"
    );
}

#[test]
fn translate_null_input_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT translate(NULL, 'a', 'b')").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn translate_null_from_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT translate('hi', NULL, 'b')").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn translate_null_to_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT translate('hi', 'a', NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn translate_arity_too_few_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT translate('a', 'b')").is_err());
}

#[test]
fn translate_arity_too_many_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT translate('a', 'b', 'c', 'd')").is_err());
}

#[test]
fn translate_normalize_phone_number() {
    // Strip spaces / dashes / parens from a phone number.
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT translate('(555) 123-4567', ' ()-', '')"),
        "5551234567"
    );
}

#[test]
fn translate_inside_where() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, code TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 'ABC-123'), (2, 'XYZ-456')")
        .unwrap();
    let r = e
        .execute("SELECT id FROM u WHERE translate(code, '-', '') = 'ABC123'")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(1));
}

#[test]
fn translate_column_type_is_text() {
    let mut e = Engine::new();
    let r = e.execute("SELECT translate('a', 'a', 'b')").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, spg_storage::DataType::Text);
}
