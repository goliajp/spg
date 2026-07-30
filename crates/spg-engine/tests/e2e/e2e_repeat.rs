//! PG `repeat(string, n)` — duplicate a string N times.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-string.html
//!
//! Invariants pinned:
//!   * n=0 → ''
//!   * n<0 → '' (PG behavior — does NOT error)
//!   * empty input + any n → ''
//!   * Multi-byte UTF-8 preserved verbatim.
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
fn basic_repeat() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT repeat('ab', 3)"), "ababab");
}

#[test]
fn repeat_n_one_returns_input() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT repeat('hello', 1)"), "hello");
}

#[test]
fn repeat_n_zero_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT repeat('hello', 0)"), "");
}

#[test]
fn repeat_negative_n_returns_empty_not_error() {
    // PG verified: negative n yields '' (does not error).
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT repeat('hello', -1)"), "");
    assert_eq!(text(&mut e, "SELECT repeat('hello', -1000)"), "");
}

#[test]
fn repeat_empty_input_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT repeat('', 5)"), "");
}

#[test]
fn repeat_empty_with_zero_n_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT repeat('', 0)"), "");
}

#[test]
fn repeat_single_char() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT repeat('-', 10)"), "----------");
}

#[test]
fn repeat_multibyte_preserved() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT repeat('日', 3)"), "日日日");
    assert_eq!(text(&mut e, "SELECT repeat('👍', 4)"), "👍👍👍👍");
}

#[test]
fn repeat_multichar_unicode() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT repeat('日本', 3)"), "日本日本日本");
}

#[test]
fn repeat_large_n_stress() {
    // Reasonable upper bound — engines should accept large n
    // as long as the result fits in memory.
    let mut e = Engine::new();
    let r = text(&mut e, "SELECT repeat('a', 1000)");
    assert_eq!(r.len(), 1000);
}

#[test]
fn repeat_null_input_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT repeat(NULL, 3)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn repeat_null_n_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT repeat('a', NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn repeat_too_few_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT repeat('a')").is_err());
}

#[test]
fn repeat_too_many_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT repeat('a', 1, 2)").is_err());
}

#[test]
fn repeat_n_non_integer_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT repeat('a', 'foo')").is_err());
}

#[test]
fn repeat_numeric_input_coerced() {
    let mut e = Engine::new();
    // v7.39 (round 625) — PG has no `repeat(integer, integer)`.
    let m = e
        .execute("SELECT repeat(7, 3)")
        .expect_err("PG has no repeat(integer, integer)")
        .to_string();
    assert!(m.contains("function repeat(integer, integer) does not exist"), "{m}");
}

#[test]
fn repeat_inside_where_clause() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, sep TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, '-'), (2, '=')")
        .unwrap();
    let r = e
        .execute("SELECT id FROM u WHERE repeat(sep, 3) = '---'")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(1));
}

#[test]
fn repeat_inside_insert_values() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (banner TEXT NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES (repeat('=', 20))").unwrap();
    let row = one_row(e.execute("SELECT banner FROM u").unwrap());
    assert_eq!(row[0], Value::text("=".repeat(20)));
}

#[test]
fn repeat_column_type_is_text() {
    let mut e = Engine::new();
    let r = e.execute("SELECT repeat('a', 3)").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, spg_storage::DataType::Text);
}
