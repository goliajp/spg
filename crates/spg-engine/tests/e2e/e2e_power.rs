//! PG `power(x, y)` / `pow(x, y)` — x raised to y.

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

fn float_result(e: &mut Engine, sql: &str) -> f64 {
    let row = one_row(
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}")),
    );
    match &row[0] {
        Value::Float(x) => *x,
        Value::Int(n) => f64::from(*n),
        Value::BigInt(n) => *n as f64,
        Value::Numeric { scaled, scale } => (*scaled as f64) / 10f64.powi(i32::from(*scale)),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn power_2_to_8() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT power(2, 8)"), 256.0);
}

#[test]
fn power_squared() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT power(5, 2)"), 25.0);
}

#[test]
fn power_to_zero_is_one() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT power(5, 0)"), 1.0);
}

#[test]
fn power_one_to_anything_is_one() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT power(1, 100)"), 1.0);
}

#[test]
fn power_zero_to_positive_is_zero() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT power(0, 5)"), 0.0);
}

#[test]
fn power_negative_base_even_exponent_positive() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT power(-2, 4)"), 16.0);
}

#[test]
fn power_negative_base_odd_exponent_negative() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT power(-2, 3)"), -8.0);
}

#[test]
fn power_negative_exponent_yields_reciprocal() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT power(2, -3)"), 0.125);
}

#[test]
fn power_fractional_exponent_sqrt() {
    let mut e = Engine::new();
    let r = float_result(&mut e, "SELECT power(4, 0.5)");
    assert!((r - 2.0).abs() < 1e-9);
}

#[test]
fn power_pow_alias() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT pow(3, 4)"), 81.0);
}

#[test]
fn power_null_x_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT power(NULL, 2)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn power_null_y_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT power(2, NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn power_one_arg_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT power(2)").is_err());
}

#[test]
fn power_three_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT power(2, 3, 4)").is_err());
}

#[test]
fn power_text_arg_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT power('a', 2)").is_err());
}

#[test]
fn power_inside_where_for_byte_quota() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, bytes BIGINT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 1073741824), (2, 500)") // 1 GiB, 500 B
        .unwrap();
    let r = e
        .execute("SELECT id FROM u WHERE bytes >= power(2, 30)")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(1));
}
