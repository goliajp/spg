//! v7.38 (read01 sweep) — single-numeric-argument math functions coerce an
//! unknown-type string literal argument to numeric (PG's implicit unknown →
//! typed cast). Oracle behaviour from live PG 18.4.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn math_functions_coerce_string_argument() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT abs('-7')"), Value::Numeric { scaled: 7, scale: 0 });
    assert_eq!(one(&mut e, "SELECT sqrt('16')"), Value::Float(4.0));
    assert_eq!(one(&mut e, "SELECT cbrt('27')"), Value::Float(3.0));
    assert_eq!(one(&mut e, "SELECT sign('-5')"), Value::Numeric { scaled: -1, scale: 0 });
    assert_eq!(one(&mut e, "SELECT ceil('3.2')"), Value::Numeric { scaled: 4, scale: 0 });
    assert_eq!(one(&mut e, "SELECT floor('3.8')"), Value::Numeric { scaled: 3, scale: 0 });
    assert_eq!(one(&mut e, "SELECT round('3.567', 2)"), Value::Numeric { scaled: 357, scale: 2 });
}

#[test]
fn math_functions_string_edge_cases() {
    let mut e = Engine::new();
    // A non-numeric string still errors.
    assert!(e.execute("SELECT abs('xyz')").is_err());
    // Non-string args are unaffected.
    assert_eq!(one(&mut e, "SELECT abs(-7)"), Value::Int(7));
    // trunc is left ambiguous, matching PG ("function trunc(unknown) is not unique").
    assert!(e.execute("SELECT trunc('3.9')").is_err());
}
