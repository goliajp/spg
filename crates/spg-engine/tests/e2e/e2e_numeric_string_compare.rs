//! v7.38 (read01 sweep) — comparing a numeric operand with an unknown-type
//! string literal coerces the literal to the numeric type (PG's implicit
//! unknown → typed-operand cast). `WHERE id = '5'` is the ubiquitous ORM
//! shape. Oracle behaviour from live PG 18.4.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn scalar(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn numeric_compared_to_string_literal_coerces() {
    let mut e = Engine::new();
    assert_eq!(scalar(&mut e, "SELECT (1 = '1')"), Value::Bool(true));
    assert_eq!(scalar(&mut e, "SELECT (2 < '10')"), Value::Bool(true));
    assert_eq!(scalar(&mut e, "SELECT (3.5 = '3.5')"), Value::Bool(true));
    assert_eq!(
        scalar(&mut e, "SELECT (100::bigint > '50')"),
        Value::Bool(true)
    );
    // A non-numeric string still errors (coercion fails), matching PG.
    assert!(e.execute("SELECT (1 = 'abc')").is_err());
}

#[test]
fn where_int_column_equals_string_literal() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ct (i INT, t TEXT)").unwrap();
    e.execute("INSERT INTO ct VALUES (5, 'hello'), (7, 'world')")
        .unwrap();
    // The ubiquitous ORM shape: int column = quoted string.
    assert_eq!(
        scalar(&mut e, "SELECT i FROM ct WHERE i = '5'"),
        Value::Int(5)
    );
    // Text vs Text is unaffected — still a string comparison.
    assert_eq!(
        scalar(&mut e, "SELECT (t = 'hello') FROM ct WHERE i = 5"),
        Value::Bool(true)
    );
}
