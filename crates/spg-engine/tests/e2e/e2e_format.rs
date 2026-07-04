//! v7.17.0 Phase 3.8 — PG `format(fmt, args…)` sprintf-style.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn percent_s_text_substitution() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT format('Hello %s', 'world')").unwrap());
    assert_eq!(r[0][0], Value::text("Hello world"));
}

#[test]
fn percent_s_multiple_args() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT format('%s + %s = %s', 1, 2, 3)").unwrap());
    assert_eq!(r[0][0], Value::text("1 + 2 = 3"));
}

#[test]
fn percent_l_quoted_literal() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT format('WHERE name = %L', 'alice')")
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::text("WHERE name = 'alice'"));
}

#[test]
fn percent_l_escapes_single_quote() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT format('= %L', 'O''Brien')").unwrap());
    assert_eq!(r[0][0], Value::text("= 'O''Brien'"));
}

#[test]
fn percent_l_null_renders_as_NULL_literal() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT format('= %L', NULL)").unwrap());
    assert_eq!(r[0][0], Value::text("= NULL"));
}

#[test]
fn percent_capital_i_quoted_identifier() {
    // PG's %I uses quote_identifier: a safe unquoted identifier like
    // `mytable` is emitted verbatim, NOT wrapped in double quotes.
    // (Live PG18: `format('SELECT FROM %I','mytable')` = `SELECT FROM
    // mytable`.)
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT format('SELECT FROM %I', 'mytable')")
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::Text("SELECT FROM mytable".into()));
}

#[test]
fn percent_capital_i_escapes_double_quote() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT format('%I', 'has\"quote')").unwrap());
    assert_eq!(r[0][0], Value::Text(r#""has""quote""#.into()));
}

#[test]
fn percent_percent_literal() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT format('100%%')").unwrap());
    assert_eq!(r[0][0], Value::text("100%"));
}

#[test]
fn positional_argument_n_dollar() {
    // PG `format` supports `%n$X` positional refs (1-based).
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT format('%2$s %1$s', 'last', 'first')")
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::text("first last"));
}

#[test]
fn dynamic_sql_assembly_pattern() {
    // The customer use case mailrs hits: build an INSERT/UPDATE
    // query string for dynamic table/column refs.
    let mut e = Engine::new();
    let r = rows(
        e.execute(
            "SELECT format('INSERT INTO %I (%I) VALUES (%L)', \
                           'users', 'name', 'alice')",
        )
        .unwrap(),
    );
    // Live PG18: `users` / `name` are safe unquoted identifiers, so
    // %I leaves them bare; only %L quotes the literal.
    assert_eq!(
        r[0][0],
        Value::Text("INSERT INTO users (name) VALUES ('alice')".into())
    );
}

#[test]
fn unknown_specifier_errors() {
    let mut e = Engine::new();
    let r = e.execute("SELECT format('%q', 'x')");
    assert!(r.is_err());
}

#[test]
fn null_format_string_propagates() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT format(NULL, 'x')").unwrap());
    assert_eq!(r[0][0], Value::Null);
}

#[test]
fn percent_s_null_arg_renders_empty() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT format('[%s]', NULL)").unwrap());
    assert_eq!(r[0][0], Value::text("[]"));
}
