//! v7.37.17 (17.6 siblings) — to_number(text, fmt).

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn as_float(v: &spg_storage::Value<'_>) -> f64 {
    match v {
        spg_storage::Value::Float(f) => *f,
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn to_number_basic() {
    let mut e = Engine::new();
    assert_eq!(as_float(&first(&mut e, "SELECT to_number('12345', '99999')")), 12345.0);
    assert_eq!(as_float(&first(&mut e, "SELECT to_number('-42', 'S99')")), -42.0);
    assert_eq!(as_float(&first(&mut e, "SELECT to_number('3.14', '9D99')")), 3.14);
}

#[test]
fn to_number_strips_locale_chars() {
    let mut e = Engine::new();
    // '1,234.56' with comma+dot presentation.
    assert_eq!(
        as_float(&first(&mut e, "SELECT to_number('1,234.56', '9G999D99')")),
        1234.56
    );
    // '$1,234' with currency + comma.
    assert_eq!(
        as_float(&first(&mut e, "SELECT to_number('$1,234', 'L9G999')")),
        1234.0
    );
    // Scientific notation still works.
    assert_eq!(
        as_float(&first(&mut e, "SELECT to_number('1.5e3', '9D9EEEE')")),
        1500.0
    );
}

#[test]
fn to_number_empty_or_all_locale_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT to_number('$$$$', 'L9')").is_err());
    assert!(e.execute("SELECT to_number('abc', '999')").is_err());
}

#[test]
fn to_number_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT to_number(NULL::text, '999')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT to_number('1', NULL::text)"),
        spg_storage::Value::Null
    ));
}
