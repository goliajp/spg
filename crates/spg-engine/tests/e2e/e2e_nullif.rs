//! PG `nullif(a, b)` — returns NULL when a = b, else a.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-conditional.html
//!
//! Universal "guard against expected value" helper. The two
//! canonical use cases:
//!   * Divide-by-zero protection: `x / nullif(y, 0)` — yields
//!     NULL instead of an error when y is zero.
//!   * Empty-string normalisation: `nullif(form_field, '')` —
//!     coerces empty input to NULL for clean WHERE filtering.
//!
//! Invariants pinned:
//!   * a = b → NULL.
//!   * a ≠ b → a (NOT b).
//!   * NULL on a → NULL.
//!   * NULL on b → a (a is not equal to NULL via this fn).
//!   * Type comparison is strict for the most part — text vs
//!     int may or may not match depending on engine coerce path.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one_row(r: QueryResult) -> Vec<Value> {
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            rows.into_iter().next().unwrap().values
        }
        _ => panic!(),
    }
}

// ── BASIC ────────────────────────────────────────────────────────

#[test]
fn nullif_equal_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT nullif(5, 5)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn nullif_unequal_returns_first_arg() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT nullif(5, 3)").unwrap());
    // Could be Int or BigInt depending on coerce path.
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, 5),
        Value::BigInt(n) => assert_eq!(*n, 5),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn nullif_text_equal_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT nullif('hello', 'hello')").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn nullif_text_unequal_returns_first() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT nullif('hello', 'world')").unwrap());
    assert_eq!(row[0], Value::Text("hello".into()));
}

#[test]
fn nullif_empty_string_pattern() {
    // Classic: nullif('', '') → NULL.
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT nullif('', '')").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn nullif_zero_pattern() {
    // For divide-by-zero protection.
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT nullif(0, 0)").unwrap());
    assert_eq!(row[0], Value::Null);
}

// ── NULL HANDLING ────────────────────────────────────────────────

#[test]
fn nullif_null_a_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT nullif(NULL, 5)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn nullif_null_b_returns_a() {
    // PG: nullif(5, NULL) → 5 (NULL ≠ 5 per IS DISTINCT semantic)
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT nullif(5, NULL)").unwrap());
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, 5),
        Value::BigInt(n) => assert_eq!(*n, 5),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn nullif_both_null_returns_null() {
    // Edge: PG returns NULL here.
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT nullif(NULL, NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

// ── ARITY ────────────────────────────────────────────────────────

#[test]
fn nullif_one_arg_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT nullif(5)").is_err());
}

#[test]
fn nullif_three_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT nullif(5, 6, 7)").is_err());
}

#[test]
fn nullif_zero_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT nullif()").is_err());
}

// ── REALISTIC USE CASES ─────────────────────────────────────────

#[test]
fn nullif_for_divide_by_zero_protection() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE rate (num INT NOT NULL, denom INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO rate VALUES (10, 2), (5, 0)")
        .unwrap();
    let r = e
        .execute("SELECT num::FLOAT / nullif(denom, 0) FROM rate ORDER BY num DESC")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 2);
    match &rows[0].values[0] {
        Value::Float(x) => assert_eq!(*x, 5.0),
        other => panic!("got {other:?}"),
    }
    // Second row: denom=0 → nullif(0,0)=NULL → divide-by-NULL = NULL.
    assert_eq!(rows[1].values[0], Value::Null);
}

#[test]
fn nullif_empty_string_to_null() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE form (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO form VALUES (1, ''), (2, 'alice')")
        .unwrap();
    let r = e
        .execute("SELECT id, nullif(name, '') FROM form ORDER BY id")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[1], Value::Null);
    assert_eq!(rows[1].values[1], Value::Text("alice".into()));
}

#[test]
fn nullif_inside_where() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, status TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 'active'), (2, 'inactive'), (3, 'deleted')")
        .unwrap();
    // Filter out 'deleted' by mapping it to NULL then IS NULL excludes.
    let r = e
        .execute("SELECT id FROM u WHERE nullif(status, 'deleted') IS NOT NULL ORDER BY id")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[0], Value::Int(1));
    assert_eq!(rows[1].values[0], Value::Int(2));
}

#[test]
fn nullif_with_negative_numbers() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT nullif(-5, -5)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn nullif_with_floats() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT nullif(1.5, 1.5)").unwrap());
    assert_eq!(row[0], Value::Null);
    let row = one_row(e.execute("SELECT nullif(1.5, 2.5)").unwrap());
    match &row[0] {
        Value::Float(x) => assert_eq!(*x, 1.5),
        Value::Numeric { scaled, scale } => {
            assert_eq!(*scaled, 15 * 10_i128.pow(u32::from(*scale) - 1));
        }
        other => panic!("got {other:?}"),
    }
}
