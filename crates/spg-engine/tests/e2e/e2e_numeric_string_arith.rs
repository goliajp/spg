//! v7.38 (read01 sweep) — arithmetic between a numeric operand and an
//! unknown-type string literal coerces the literal to the numeric type
//! (PG's implicit unknown → typed cast). Oracle behaviour from live PG 18.4.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn scalar(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn arithmetic_with_string_literal_coerces() {
    let mut e = Engine::new();
    assert_eq!(scalar(&mut e, "SELECT 5 + '3'"), Value::Int(8));
    assert_eq!(scalar(&mut e, "SELECT '10' - 2"), Value::Int(8));
    assert_eq!(scalar(&mut e, "SELECT 2 * '4'"), Value::Int(8));
    assert_eq!(scalar(&mut e, "SELECT '10' / 4"), Value::Int(2));
    assert_eq!(scalar(&mut e, "SELECT 10 % '3'"), Value::Int(1));
    assert_eq!(
        scalar(&mut e, "SELECT 1.5 + '2'"),
        Value::Numeric {
            scaled: 35,
            scale: 1,
            kind: spg_storage::NumericKind::Finite
        }
    );
}

#[test]
fn arithmetic_string_edge_cases() {
    let mut e = Engine::new();
    // A non-numeric string still errors (coercion fails), matching PG.
    assert!(e.execute("SELECT 5 + 'abc'").is_err());
    // Two text operands with no numeric side stay a type error.
    assert!(e.execute("SELECT 'a' - 'b'").is_err());
    // Concatenation is unaffected — number folds into text.
    assert_eq!(scalar(&mut e, "SELECT 'x' || 5"), Value::text("x5"));
}
