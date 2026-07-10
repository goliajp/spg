//! v7.38 (read01 sweep) — comparing a bool operand with an unknown-type string
//! literal coerces the literal to bool (PG's implicit unknown → typed cast),
//! accepting PG's full bool-literal set. Oracle behaviour from live PG 18.4.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn b(e: &mut Engine, sql: &str) -> bool {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Bool(v) => *v,
            v => panic!("expected bool, got {v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn bool_compared_to_string_literal_coerces() {
    let mut e = Engine::new();
    for lit in ["t", "true", "yes", "on", "1"] {
        assert!(
            b(&mut e, &format!("SELECT true = '{lit}'")),
            "true = '{lit}'"
        );
    }
    for lit in ["f", "no", "0"] {
        assert!(
            b(&mut e, &format!("SELECT false = '{lit}'")),
            "false = '{lit}'"
        );
    }
    // A non-bool string still errors, matching PG.
    assert!(e.execute("SELECT true = 'maybe'").is_err());
}

#[test]
fn where_bool_column_equals_string_literal() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE bt (id INT, flag BOOL)").unwrap();
    e.execute("INSERT INTO bt VALUES (1, true), (2, false)")
        .unwrap();
    match e.execute("SELECT id FROM bt WHERE flag = 't'").unwrap() {
        QueryResult::Rows { rows, .. } => assert_eq!(rows[0].values[0], Value::Int(1)),
        _ => panic!(),
    }
}
