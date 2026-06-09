//! PG `concat_ws(sep, val1 [, val2 ...])` — variadic with
//! separator. Semantic subtleties tested here against PG
//! reference behavior.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-string.html
//!   "Concatenates all arguments but the first with separators.
//!    The first argument is used as the separator string, and
//!    should not be NULL. Other NULL arguments are ignored."
//!
//! Key contrasts with concat():
//!   * concat_ws(NULL, 'a', 'b') → NULL  (separator-NULL poisons)
//!   * concat_ws(',', 'a', NULL, 'b') → 'a,b'  (NULL arg dropped;
//!     separator NOT doubled on the NULL gap)
//!   * concat_ws(',') with 0 data args → ''
//!   * concat_ws(',', NULL) → '' (single NULL arg → empty,
//!     NOT 'NULL')

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one_row(r: QueryResult) -> Vec<Value> {
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "expected exactly 1 row");
            rows.into_iter().next().unwrap().values
        }
        _ => panic!("expected rows"),
    }
}

fn first_text(e: &mut Engine, sql: &str) -> Value {
    let r = e.execute(sql).unwrap_or_else(|err| {
        panic!("execute({sql:?}) failed: {err:?}");
    });
    one_row(r).into_iter().next().unwrap()
}

// ── ARITY ─────────────────────────────────────────────────────────

#[test]
fn separator_only_returns_empty_string() {
    // PG: SELECT concat_ws(',') → ''
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws(',')"),
        Value::Text("".into())
    );
}

#[test]
fn one_data_arg_no_separator_emitted() {
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws(',', 'only')"),
        Value::Text("only".into())
    );
}

#[test]
fn two_data_args_separator_between() {
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws(',', 'a', 'b')"),
        Value::Text("a,b".into())
    );
}

#[test]
fn five_data_args() {
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws('-', 'a', 'b', 'c', 'd', 'e')"),
        Value::Text("a-b-c-d-e".into())
    );
}

#[test]
fn zero_args_after_sep_is_error_or_empty() {
    // PG: concat_ws() (no args at all) is an arity error. We
    // require sep to be present. Pin behavior either way — a
    // clean error or empty result is acceptable; a panic is not.
    let mut e = Engine::new();
    let r = e.execute("SELECT concat_ws()");
    assert!(r.is_err(), "concat_ws with 0 args should error");
}

// ── SEPARATOR-IS-NULL POISONS RESULT ──────────────────────────────

#[test]
fn null_separator_returns_null() {
    // PG verified: NULL sep poisons the whole result.
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws(NULL, 'a', 'b')"),
        Value::Null
    );
}

#[test]
fn null_separator_returns_null_even_when_only_one_data_arg() {
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws(NULL, 'lonely')"),
        Value::Null
    );
}

#[test]
fn null_separator_returns_null_with_no_data_args() {
    let mut e = Engine::new();
    assert_eq!(first_text(&mut e, "SELECT concat_ws(NULL)"), Value::Null);
}

// ── NULL DATA ARGS SKIPPED (NOT WIDENED INTO SEPARATOR REPEATS) ──

#[test]
fn null_data_arg_skipped_separator_not_doubled() {
    // PG verified: concat_ws(',', 'a', NULL, 'b') → 'a,b'
    // NOT 'a,,b' (no doubled separator on the NULL gap).
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws(',', 'a', NULL, 'b')"),
        Value::Text("a,b".into())
    );
}

#[test]
fn all_data_args_null_returns_empty_string() {
    // PG: concat_ws(',', NULL, NULL) → '' (NOT NULL, distinct
    // from null-sep poisoning).
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws(',', NULL, NULL)"),
        Value::Text("".into())
    );
}

#[test]
fn one_null_data_arg_returns_empty_string() {
    // PG: concat_ws(',', NULL) → ''
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws(',', NULL)"),
        Value::Text("".into())
    );
}

#[test]
fn leading_nulls_followed_by_value() {
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws(',', NULL, NULL, 'first')"),
        Value::Text("first".into())
    );
}

#[test]
fn trailing_nulls_after_value() {
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws(',', 'last', NULL, NULL)"),
        Value::Text("last".into())
    );
}

#[test]
fn alternating_nulls_and_values() {
    let mut e = Engine::new();
    assert_eq!(
        first_text(
            &mut e,
            "SELECT concat_ws('-', NULL, 'a', NULL, 'b', NULL, 'c')"
        ),
        Value::Text("a-b-c".into())
    );
}

// ── SEPARATOR VARIANTS ────────────────────────────────────────────

#[test]
fn empty_string_separator_joins_with_no_glue() {
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws('', 'a', 'b', 'c')"),
        Value::Text("abc".into())
    );
}

#[test]
fn multichar_separator() {
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws(', ', 'a', 'b', 'c')"),
        Value::Text("a, b, c".into())
    );
}

#[test]
fn multibyte_utf8_separator() {
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws('・', 'あ', 'い', 'う')"),
        Value::Text("あ・い・う".into())
    );
}

// ── EMPTY-STRING DATA vs NULL DATA ────────────────────────────────

#[test]
fn empty_string_data_arg_kept_separator_around_it() {
    // PG: concat_ws(',', 'a', '', 'b') → 'a,,b' — the empty
    // string IS a value (just zero-width); separator goes
    // around it.
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws(',', 'a', '', 'b')"),
        Value::Text("a,,b".into())
    );
}

// ── TYPE COERCION ────────────────────────────────────────────────

#[test]
fn integer_data_args_coerced() {
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws('-', 1, 2, 3)"),
        Value::Text("1-2-3".into())
    );
}

#[test]
fn mixed_types_coerced() {
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws('|', 'a', 1, true, NULL, 2.5)"),
        Value::Text("a|1|t|2.5".into())
    );
}

#[test]
fn integer_separator_coerced_to_text() {
    let mut e = Engine::new();
    // PG accepts non-text separator (numeric coerces). Edge case
    // — apps rarely emit this but the engine should not panic.
    assert_eq!(
        first_text(&mut e, "SELECT concat_ws(0, 'a', 'b')"),
        Value::Text("a0b".into())
    );
}

// ── COLUMN-LEVEL BEHAVIOR ─────────────────────────────────────────

#[test]
fn concat_ws_over_nullable_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (first TEXT, middle TEXT, last TEXT)")
        .unwrap();
    e.execute("INSERT INTO p VALUES ('John', NULL, 'Doe')")
        .unwrap();
    let row = one_row(
        e.execute("SELECT concat_ws(' ', first, middle, last) FROM p")
            .unwrap(),
    );
    assert_eq!(row[0], Value::Text("John Doe".into()));
}

#[test]
fn concat_ws_full_name_with_all_null_returns_empty() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (first TEXT, last TEXT)").unwrap();
    e.execute("INSERT INTO p VALUES (NULL, NULL)").unwrap();
    let row = one_row(
        e.execute("SELECT concat_ws(' ', first, last) FROM p")
            .unwrap(),
    );
    assert_eq!(row[0], Value::Text("".into()));
}

// ── INSERT + COMPOSITION ──────────────────────────────────────────

#[test]
fn concat_ws_inside_insert_values() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (label TEXT NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES (concat_ws('-', 'X', 42, 'Y'))")
        .unwrap();
    let row = one_row(e.execute("SELECT label FROM u").unwrap());
    assert_eq!(row[0], Value::Text("X-42-Y".into()));
}

#[test]
fn concat_ws_inside_where() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (a TEXT NOT NULL, b TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES ('foo', 'bar'), ('baz', 'qux')")
        .unwrap();
    let r = e
        .execute("SELECT a FROM u WHERE concat_ws('|', a, b) = 'foo|bar'")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Text("foo".into()));
}

#[test]
fn concat_ws_nested() {
    let mut e = Engine::new();
    assert_eq!(
        first_text(
            &mut e,
            "SELECT concat_ws('->', concat_ws(':', 'a', 'b'), concat_ws(':', 'c', 'd'))"
        ),
        Value::Text("a:b->c:d".into())
    );
}

// ── DIFFERENTIATION FROM concat() ─────────────────────────────────

#[test]
fn concat_ws_null_sep_differs_from_concat() {
    // Sanity that concat behaves differently — concat skips NULL,
    // even where concat_ws would poison.
    let mut e = Engine::new();
    assert_eq!(
        first_text(&mut e, "SELECT concat(NULL, 'a', 'b')"),
        Value::Text("ab".into())
    );
    // concat_ws's NULL-sep poison is in null_separator_returns_null.
}
