//! PG `split_part(string, delimiter, n)` — split + index.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-string.html
//!   "Splits string at occurrences of delimiter and returns the
//!    n'th field (counting from one), or when n is negative,
//!    returns the |n|'th-from-last field."
//!
//! Key invariants pinned here:
//!   * 1-indexed (`n=1` is the first field).
//!   * `n=0` → error (PG verified).
//!   * Negative n → count from end (PG 14+).
//!   * Out-of-range n → empty string '' (NOT NULL).
//!   * Empty delimiter → error (PG verified).
//!   * Empty input + non-empty delim → '' for n=1, '' for n out of range.
//!   * Delim not present + n=1 → returns the whole string.
//!   * Multi-char delim works (e.g. '::' as separator).

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

// ── BASIC 1-INDEXED ──────────────────────────────────────────────

#[test]
fn split_part_first_field() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('a,b,c', ',', 1)"), "a");
}

#[test]
fn split_part_middle_field() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('a,b,c', ',', 2)"), "b");
}

#[test]
fn split_part_last_field() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('a,b,c', ',', 3)"), "c");
}

// ── OUT-OF-RANGE → EMPTY ─────────────────────────────────────────

#[test]
fn split_part_n_past_end_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('a,b,c', ',', 4)"), "");
}

#[test]
fn split_part_n_far_past_end_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('a,b,c', ',', 9999)"), "");
}

// ── NEGATIVE N (PG 14+) ──────────────────────────────────────────

#[test]
fn split_part_n_negative_counts_from_end() {
    // PG 14+: n=-1 is last field.
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('a,b,c', ',', -1)"), "c");
}

#[test]
fn split_part_n_negative_two_is_second_from_end() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('a,b,c', ',', -2)"), "b");
}

#[test]
fn split_part_n_negative_three_is_third_from_end() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('a,b,c', ',', -3)"), "a");
}

#[test]
fn split_part_n_negative_past_start_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('a,b,c', ',', -99)"), "");
}

// ── n=0 ERRORS ───────────────────────────────────────────────────

#[test]
fn split_part_n_zero_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT split_part('a,b,c', ',', 0)").is_err());
}

// ── EMPTY DELIMITER ERRORS ───────────────────────────────────────

#[test]
fn split_part_empty_delim_is_whole_string() {
    // v7.39 (read01 varlena.c) — PG treats an empty delimiter as "the
    // whole string is field 1" (also -1); other fields are ''.
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('abc', '', 1)"), "abc");
    assert_eq!(text(&mut e, "SELECT split_part('abc', '', -1)"), "abc");
    assert_eq!(text(&mut e, "SELECT split_part('abc', '', 2)"), "");
}

// ── DELIM NOT PRESENT ────────────────────────────────────────────

#[test]
fn split_part_no_delim_in_input_n1_returns_whole_string() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT split_part('abcdef', ',', 1)"),
        "abcdef"
    );
}

#[test]
fn split_part_no_delim_in_input_n2_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('abcdef', ',', 2)"), "");
}

#[test]
fn split_part_no_delim_n_negative_one_returns_whole_string() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT split_part('abcdef', ',', -1)"),
        "abcdef"
    );
}

// ── EMPTY INPUT ──────────────────────────────────────────────────

#[test]
fn split_part_empty_input_n1_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('', ',', 1)"), "");
}

#[test]
fn split_part_empty_input_n2_returns_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('', ',', 2)"), "");
}

// ── EMPTY FIELDS PRESERVED ───────────────────────────────────────

#[test]
fn split_part_consecutive_delims_yield_empty_field() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('a,,b', ',', 2)"), "");
}

#[test]
fn split_part_leading_delim_field_1_is_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part(',a,b', ',', 1)"), "");
}

#[test]
fn split_part_trailing_delim_last_field_empty() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('a,b,', ',', 3)"), "");
}

// ── MULTI-CHAR DELIM ─────────────────────────────────────────────

#[test]
fn split_part_multi_char_delim() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('a::b::c', '::', 2)"), "b");
}

#[test]
fn split_part_pipe_delim() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('a|b|c', '|', 3)"), "c");
}

// ── UNICODE ──────────────────────────────────────────────────────

#[test]
fn split_part_unicode_input() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT split_part('日本,東京,京都', ',', 2)"),
        "東京"
    );
}

#[test]
fn split_part_unicode_delim() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT split_part('a・b・c', '・', 2)"), "b");
}

// ── NULL HANDLING ────────────────────────────────────────────────

#[test]
fn split_part_null_input_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT split_part(NULL, ',', 1)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn split_part_null_delim_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT split_part('a,b', NULL, 1)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn split_part_null_n_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT split_part('a,b', ',', NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

// ── ARITY ────────────────────────────────────────────────────────

#[test]
fn split_part_too_few_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT split_part('a', ',')").is_err());
}

#[test]
fn split_part_too_many_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT split_part('a', ',', 1, 2)").is_err());
}

// ── REAL-WORLD ──────────────────────────────────────────────────

#[test]
fn split_part_parse_uri_path_segment() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT split_part('/api/v1/users', '/', 3)"),
        "v1"
    );
}

#[test]
fn split_part_get_file_extension() {
    let mut e = Engine::new();
    // 'foo.tar.gz' → -1th '.'-field = 'gz'.
    assert_eq!(
        text(&mut e, "SELECT split_part('foo.tar.gz', '.', -1)"),
        "gz"
    );
}

#[test]
fn split_part_get_filename_without_extension_using_negative() {
    // Get everything except the last extension via a combined
    // shape isn't trivial — at least the components.
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT split_part('foo.tar.gz', '.', 1)"),
        "foo"
    );
}

// ── INSIDE WHERE / INSERT ────────────────────────────────────────

#[test]
fn split_part_inside_where_clause() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, csv TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 'alice,30,engineer'), (2, 'bob,25,artist')")
        .unwrap();
    let r = e
        .execute("SELECT id FROM u WHERE split_part(csv, ',', 1) = 'alice'")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(1));
}

#[test]
fn split_part_inside_insert_values() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (first TEXT NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES (split_part('John Doe', ' ', 1))")
        .unwrap();
    let row = one_row(e.execute("SELECT first FROM u").unwrap());
    assert_eq!(row[0], Value::text("John"));
}

// ── TYPE / METADATA ──────────────────────────────────────────────

#[test]
fn split_part_column_type_is_text() {
    let mut e = Engine::new();
    let r = e.execute("SELECT split_part('a,b', ',', 1)").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, spg_storage::DataType::Text);
}
