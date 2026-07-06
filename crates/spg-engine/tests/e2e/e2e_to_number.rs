//! v7.37.17 (17.6 siblings) — to_number(text, fmt).

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

// v7.38 (read01 P6.02) — to_number now returns `numeric` (PG parity), so
// decode the exact (scaled, scale) to f64 for these value comparisons.
fn as_float(v: &spg_storage::Value<'_>) -> f64 {
    match v {
        spg_storage::Value::Float(f) => *f,
        spg_storage::Value::Numeric { scaled, scale } => {
            *scaled as f64 / 10f64.powi(i32::from(*scale))
        }
        other => panic!("expected numeric, got {other:?}"),
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

#[test]
fn to_number_returns_numeric_with_full_precision() {
    // v7.38 (read01 P6.02) — PG's to_number returns `numeric`, not float, so
    // high-precision inputs keep every digit.
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT to_number('1.5', 'FM9.9')"),
        spg_storage::Value::Numeric { scaled: 15, scale: 1 }
    ));
    // 20-digit input survives (would have been mangled through f64).
    assert!(matches!(
        first(
            &mut e,
            "SELECT to_number('1234567890123456789.12', 'FM999999999999999999.99')"
        ),
        spg_storage::Value::Numeric { scaled: 123456789012345678912, scale: 2 }
    ));
    // Numeric arithmetic on the result stays exact.
    assert!(matches!(
        first(&mut e, "SELECT to_number('0.1','9.9') + to_number('0.2','9.9')"),
        spg_storage::Value::Numeric { scaled: 3, scale: 1 }
    ));
}
