//! MySQL `ifnull(a, b)` and `if(cond, then, else)`.
//!
//! ifnull(a, b) ≡ coalesce(a, b) (2-arg form).
//! if(cond, then, else) ≡ CASE WHEN cond THEN then ELSE else END.
//!
//! These MySQL-specific aliases are emitted by Hibernate / Laravel /
//! Sequelize / pretty much every ORM with a MySQL target. Required
//! for 0-change cutover from MySQL.

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

// ── ifnull(a, b) ─────────────────────────────────────────────────

#[test]
fn ifnull_first_non_null_returned() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT ifnull('alice', 'default')").unwrap());
    assert_eq!(row[0], Value::text("alice"));
}

#[test]
fn ifnull_null_first_returns_second() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT ifnull(NULL, 'default')").unwrap());
    assert_eq!(row[0], Value::text("default"));
}

#[test]
fn ifnull_both_null_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT ifnull(NULL, NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn ifnull_integer_args() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT ifnull(NULL, 42)").unwrap());
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, 42),
        Value::BigInt(n) => assert_eq!(*n, 42),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn ifnull_arity_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT ifnull('a')").is_err());
    assert!(e.execute("SELECT ifnull('a', 'b', 'c')").is_err());
}

#[test]
fn ifnull_equivalent_to_two_arg_coalesce() {
    let mut e = Engine::new();
    let a = one_row(e.execute("SELECT ifnull(NULL, 'x')").unwrap());
    let b = one_row(e.execute("SELECT coalesce(NULL, 'x')").unwrap());
    assert_eq!(a, b);
}

// ── if(cond, then, else) ─────────────────────────────────────────

#[test]
fn if_true_returns_then_branch() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT if(1=1, 'yes', 'no')").unwrap());
    assert_eq!(row[0], Value::text("yes"));
}

#[test]
fn if_false_returns_else_branch() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT if(1=2, 'yes', 'no')").unwrap());
    assert_eq!(row[0], Value::text("no"));
}

#[test]
fn if_with_literal_true_boolean() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT if(true, 'a', 'b')").unwrap());
    assert_eq!(row[0], Value::text("a"));
}

#[test]
fn if_with_literal_false_boolean() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT if(false, 'a', 'b')").unwrap());
    assert_eq!(row[0], Value::text("b"));
}

#[test]
fn if_null_condition_treated_as_false() {
    // MySQL: NULL condition → else branch.
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT if(NULL, 'a', 'b')").unwrap());
    assert_eq!(row[0], Value::text("b"));
}

#[test]
fn if_with_int_condition_nonzero_is_true() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT if(1, 'a', 'b')").unwrap());
    assert_eq!(row[0], Value::text("a"));
}

#[test]
fn if_with_int_condition_zero_is_false() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT if(0, 'a', 'b')").unwrap());
    assert_eq!(row[0], Value::text("b"));
}

#[test]
fn if_returns_integer_values() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT if(1=1, 100, 200)").unwrap());
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, 100),
        Value::BigInt(n) => assert_eq!(*n, 100),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn if_with_null_then_arg_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT if(true, NULL, 'else')").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn if_arity_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT if(true, 'a')").is_err());
    assert!(e.execute("SELECT if(true)").is_err());
    assert!(e.execute("SELECT if()").is_err());
}

#[test]
fn if_inside_select_projection() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, n INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 5), (2, 15), (3, 25)")
        .unwrap();
    let r = e
        .execute("SELECT id, if(n > 10, 'big', 'small') FROM u ORDER BY id")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows[0].values[1], Value::text("small"));
    assert_eq!(rows[1].values[1], Value::text("big"));
    assert_eq!(rows[2].values[1], Value::text("big"));
}
