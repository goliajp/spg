//! v7.17.0 Phase 3.8 — PG `format(fmt, args…)` sprintf-style.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn percent_s_text_substitution() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT format('Hello %s', 'world')").unwrap());
    assert_eq!(r[0][0], Value::Text("Hello world".into()));
}

#[test]
fn percent_s_multiple_args() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT format('%s + %s = %s', 1, 2, 3)").unwrap(),
    );
    assert_eq!(r[0][0], Value::Text("1 + 2 = 3".into()));
}

#[test]
fn percent_l_quoted_literal() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT format('WHERE name = %L', 'alice')").unwrap(),
    );
    assert_eq!(r[0][0], Value::Text("WHERE name = 'alice'".into()));
}

#[test]
fn percent_l_escapes_single_quote() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT format('= %L', 'O''Brien')").unwrap(),
    );
    assert_eq!(r[0][0], Value::Text("= 'O''Brien'".into()));
}

#[test]
fn percent_l_null_renders_as_NULL_literal() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT format('= %L', NULL)").unwrap());
    assert_eq!(r[0][0], Value::Text("= NULL".into()));
}

#[test]
fn percent_capital_i_quoted_identifier() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT format('SELECT FROM %I', 'mytable')").unwrap(),
    );
    assert_eq!(r[0][0], Value::Text(r#"SELECT FROM "mytable""#.into()));
}

#[test]
fn percent_capital_i_escapes_double_quote() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT format('%I', 'has\"quote')").unwrap(),
    );
    assert_eq!(r[0][0], Value::Text(r#""has""quote""#.into()));
}

#[test]
fn percent_percent_literal() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT format('100%%')").unwrap());
    assert_eq!(r[0][0], Value::Text("100%".into()));
}

#[test]
fn positional_argument_n_dollar() {
    // PG `format` supports `%n$X` positional refs (1-based).
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT format('%2$s %1$s', 'last', 'first')").unwrap(),
    );
    assert_eq!(r[0][0], Value::Text("first last".into()));
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
    assert_eq!(
        r[0][0],
        Value::Text(r#"INSERT INTO "users" ("name") VALUES ('alice')"#.into())
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
    assert_eq!(r[0][0], Value::Text("[]".into()));
}
