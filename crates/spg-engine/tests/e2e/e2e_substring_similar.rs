//! v7.38 (read01, T17) — SQL-standard substring(text FROM sql_regex FOR escape):
//! SIMILAR-TO pattern with `<esc>"..."<esc>"` delimiting the returned portion.
//! Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn opt(e: &mut Engine, sql: &str) -> Option<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => Some(s.to_string()),
            spg_storage::Value::Null => None,
            v => Some(format!("{v:?}")),
        },
        _ => panic!("rows"),
    }
}

#[test]
fn substring_similar_for_escape() {
    let mut e = Engine::new();
    assert_eq!(opt(&mut e, r#"SELECT substring('foobar' FROM '%#"o_b#"%' FOR '#')"#).as_deref(), Some("oob"));
    // The surrounding `%` yields to the captured portion (full digit run).
    assert_eq!(opt(&mut e, r#"SELECT substring('abc123def' FROM '%#"[0-9]+#"%' FOR '#')"#).as_deref(), Some("123"));
    assert_eq!(opt(&mut e, r#"SELECT substring('xxhelloxx' FROM '%#"hello#"%' FOR '#')"#).as_deref(), Some("hello"));
    assert_eq!(opt(&mut e, r#"SELECT substring('2024-06-15' FROM '#"[0-9]+#"-%' FOR '#')"#).as_deref(), Some("2024"));
    // No match → NULL.
    assert_eq!(opt(&mut e, r#"SELECT substring('foobar' FROM '%#"xyz#"%' FOR '#')"#), None);
    // The 2-arg POSIX form and the positional (numeric FOR len) form are unchanged.
    assert_eq!(opt(&mut e, "SELECT substring('foobar' FROM 'o.b')").as_deref(), Some("oob"));
    assert_eq!(opt(&mut e, "SELECT substring('foobar' FROM 3 FOR 2)").as_deref(), Some("ob"));
}
