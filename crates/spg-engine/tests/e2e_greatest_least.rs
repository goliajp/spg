//! PG `greatest(...)` / `least(...)` — variadic max/min.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-conditional.html
//!
//! Invariants pinned:
//!   * Variadic (1+ args). 0 args → error.
//!   * NULL args silently SKIPPED (PG-canonical — distinct from
//!     standard MAX/MIN aggregate which would also skip NULL).
//!   * If ALL args are NULL → NULL.
//!   * 1 non-NULL arg → that arg.
//!   * Works across numeric widths (Int, Float, NUMERIC) and
//!     strings; PG enforces type consistency via implicit cast,
//!     SPG mirrors via the cross-type widening helper.

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

// ── greatest() ───────────────────────────────────────────────────

#[test]
fn greatest_basic_three_ints() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT greatest(1, 5, 3)").unwrap());
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, 5),
        Value::BigInt(n) => assert_eq!(*n, 5),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn greatest_negative_numbers() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT greatest(-5, -1, -3)").unwrap());
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, -1),
        Value::BigInt(n) => assert_eq!(*n, -1),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn greatest_two_args() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT greatest(7, 3)").unwrap());
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, 7),
        Value::BigInt(n) => assert_eq!(*n, 7),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn greatest_single_arg_returns_arg() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT greatest(42)").unwrap());
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, 42),
        Value::BigInt(n) => assert_eq!(*n, 42),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn greatest_strings_lexicographic() {
    let mut e = Engine::new();
    let row = one_row(
        e.execute("SELECT greatest('apple', 'banana', 'cherry')").unwrap(),
    );
    assert_eq!(row[0], Value::Text("cherry".into()));
}

#[test]
fn greatest_with_floats() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT greatest(1.5, 2.5, 0.5)").unwrap());
    match &row[0] {
        Value::Float(x) => assert_eq!(*x, 2.5),
        Value::Numeric { scaled, scale } => {
            // 2.5 expressed as Numeric.
            let v = (*scaled as f64) / 10f64.powi(i32::from(*scale));
            assert_eq!(v, 2.5);
        }
        other => panic!("got {other:?}"),
    }
}

// ── least() ──────────────────────────────────────────────────────

#[test]
fn least_basic_three_ints() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT least(1, 5, 3)").unwrap());
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, 1),
        Value::BigInt(n) => assert_eq!(*n, 1),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn least_negative_numbers() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT least(-5, -1, -3)").unwrap());
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, -5),
        Value::BigInt(n) => assert_eq!(*n, -5),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn least_strings_lexicographic() {
    let mut e = Engine::new();
    let row = one_row(
        e.execute("SELECT least('banana', 'apple', 'cherry')").unwrap(),
    );
    assert_eq!(row[0], Value::Text("apple".into()));
}

// ── NULL ARGS SKIPPED ────────────────────────────────────────────

#[test]
fn greatest_null_args_silently_skipped() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT greatest(NULL, 5, NULL, 3)").unwrap());
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, 5),
        Value::BigInt(n) => assert_eq!(*n, 5),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn least_null_args_silently_skipped() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT least(NULL, 5, NULL, 3)").unwrap());
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, 3),
        Value::BigInt(n) => assert_eq!(*n, 3),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn greatest_all_null_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT greatest(NULL, NULL, NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn least_all_null_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT least(NULL, NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn greatest_single_null_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT greatest(NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

// ── ARITY ────────────────────────────────────────────────────────

#[test]
fn greatest_zero_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT greatest()").is_err());
}

#[test]
fn least_zero_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT least()").is_err());
}

// ── CROSS-TYPE WIDENING ──────────────────────────────────────────

#[test]
fn greatest_mixed_widths_widened_to_bigint() {
    let mut e = Engine::new();
    // i32 max vs i64 = max wins.
    let row = one_row(e.execute("SELECT greatest(5, 9999999999)").unwrap());
    match &row[0] {
        Value::BigInt(n) => assert_eq!(*n, 9999999999),
        other => panic!("expected BigInt, got {other:?}"),
    }
}

// ── INTEGRATION ─────────────────────────────────────────────────

#[test]
fn greatest_inside_where() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, a INT NOT NULL, b INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 5, 3), (2, 1, 9)").unwrap();
    let r = e
        .execute("SELECT id FROM u WHERE greatest(a, b) >= 9")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(2));
}

#[test]
fn least_inside_where_for_min_cap() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, n INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 5), (2, 15), (3, 25)")
        .unwrap();
    // Cap to max 10.
    let r = e
        .execute("SELECT id, least(n, 10) FROM u ORDER BY id")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows[0].values[1], Value::Int(5));
    assert_eq!(rows[1].values[1], Value::Int(10));
    assert_eq!(rows[2].values[1], Value::Int(10));
}
