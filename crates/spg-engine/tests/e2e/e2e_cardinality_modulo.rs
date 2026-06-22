//! v7.37.7 C.1.6 + C.1.7 — `cardinality(array)` builtin + `%` modulo
//! operator. Both are baseline PG scalar surface that the SPG baseline
//! corpus probe surfaced as missing on hotfix v7.37.6 tip.

use spg_engine::{Engine, EngineError, QueryResult};
use spg_storage::Value;

fn first_value(r: QueryResult) -> Value<'static> {
    match r {
        QueryResult::Rows { rows, .. } => {
            assert!(!rows.is_empty(), "expected at least one row");
            rows[0].values[0].clone()
        }
        _ => panic!("expected Rows"),
    }
}

#[test]
fn cardinality_on_int_array() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT cardinality(ARRAY[10, 20, 30, 40])")
        .expect("cardinality on int array parses + executes");
    assert_eq!(first_value(r), Value::Int(4));
}

#[test]
fn cardinality_on_text_array() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT cardinality(ARRAY['a', 'b', 'c'])")
        .expect("cardinality on text array");
    assert_eq!(first_value(r), Value::Int(3));
}

#[test]
fn cardinality_null_array_returns_null() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT cardinality(CAST(NULL AS INT[]))")
        .expect("cardinality on NULL array");
    assert_eq!(first_value(r), Value::Null);
}

#[test]
fn cardinality_wrong_arg_count_errors() {
    let mut e = Engine::new();
    let err = e.execute("SELECT cardinality(ARRAY[1], 2)").expect_err(
        "cardinality takes exactly 1 arg, mismatch must surface",
    );
    let msg = format!("{err:?}");
    assert!(
        msg.contains("cardinality"),
        "error should mention cardinality, got: {msg}"
    );
}

#[test]
fn modulo_basic_int() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT 10 % 3")
        .expect("`%` parses + executes on integers");
    assert_eq!(first_value(r), Value::Int(1));
}

#[test]
fn modulo_zero_divisor_errors() {
    let mut e = Engine::new();
    let err = e
        .execute("SELECT 5 % 0")
        .expect_err("division by zero via %");
    assert!(matches!(
        err,
        EngineError::Eval(spg_engine::eval::EvalError::DivisionByZero)
    ));
}

#[test]
fn modulo_precedence_with_multiplication() {
    let mut e = Engine::new();
    // `2 + 10 % 3 * 2` — `%` and `*` same precedence, left-to-right.
    // 10 % 3 = 1, 1 * 2 = 2, 2 + 2 = 4.
    let r = e.execute("SELECT 2 + 10 % 3 * 2").expect("precedence");
    assert_eq!(first_value(r), Value::Int(4));
}

#[test]
fn modulo_on_negative_uses_rem_euclid() {
    let mut e = Engine::new();
    // `rem_euclid` semantics: result has the sign of the divisor.
    //   (-7) rem_euclid 3 = 2
    let r = e.execute("SELECT (-7) % 3").expect("negative dividend");
    assert_eq!(first_value(r), Value::Int(2));
}

#[test]
fn modulo_in_select_against_table_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE m (id INT NOT NULL, v INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO m VALUES (1, 7), (2, 8), (3, 9), (4, 10)")
        .unwrap();
    let r = e
        .execute("SELECT id FROM m WHERE v % 2 = 0 ORDER BY id")
        .expect("`%` in WHERE filters evens");
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2, "expected v=8 (id=2) and v=10 (id=4)");
            assert_eq!(rows[0].values[0], Value::Int(2));
            assert_eq!(rows[1].values[0], Value::Int(4));
        }
        _ => panic!("Rows"),
    }
}
