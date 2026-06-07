//! PG `ceil(x)` / `ceiling(x)` — smallest integer >= x.

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

fn float_result(e: &mut Engine, sql: &str) -> f64 {
    let row = one_row(e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")));
    match &row[0] {
        Value::Float(x) => *x,
        other => panic!("expected Float, got {other:?}"),
    }
}

// ── BASIC ────────────────────────────────────────────────────────

#[test]
fn ceil_positive_fraction() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT ceil(1.3)"), 2.0);
}

#[test]
fn ceil_positive_half_step() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT ceil(1.5)"), 2.0);
}

#[test]
fn ceil_just_above_integer_positive() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT ceil(2.001)"), 3.0);
}

#[test]
fn ceil_already_integer_float() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT ceil(5.0)"), 5.0);
}

#[test]
fn ceil_zero() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT ceil(0.0)"), 0.0);
}

// ── NEGATIVE — round toward +infinity (i.e. toward zero) ────────

#[test]
fn ceil_negative_fraction_rounds_toward_zero() {
    // CRITICAL: ceil(-1.5) → -1 (round UP, toward +inf), NOT -2.
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT ceil(-1.5)"), -1.0);
}

#[test]
fn ceil_negative_half_step() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT ceil(-0.5)"), 0.0);
}

#[test]
fn ceil_negative_just_below_integer() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT ceil(-2.001)"), -2.0);
}

#[test]
fn ceil_negative_just_above_integer() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT ceil(-2.999)"), -2.0);
}

// ── INTEGER PASSTHROUGH ──────────────────────────────────────────

#[test]
fn ceil_integer_passthrough() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT ceil(42)").unwrap());
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, 42),
        Value::BigInt(n) => assert_eq!(*n, 42),
        Value::Float(x) => assert_eq!(*x, 42.0),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn ceil_negative_integer_passthrough() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT ceil(-7)").unwrap());
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, -7),
        Value::BigInt(n) => assert_eq!(*n, -7),
        Value::Float(x) => assert_eq!(*x, -7.0),
        other => panic!("got {other:?}"),
    }
}

// ── CEILING ALIAS ────────────────────────────────────────────────

#[test]
fn ceiling_is_alias_for_ceil() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT ceiling(1.3)"), 2.0);
    assert_eq!(float_result(&mut e, "SELECT ceiling(-1.5)"), -1.0);
}

// ── NULL / ARITY ─────────────────────────────────────────────────

#[test]
fn ceil_null_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT ceil(NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn ceil_zero_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT ceil()").is_err());
}

#[test]
fn ceil_too_many_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT ceil(1.5, 2)").is_err());
}

#[test]
fn ceil_text_arg_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT ceil('hello')").is_err());
}

// ── COLUMN TYPE ──────────────────────────────────────────────────

#[test]
fn ceil_column_type_is_float_for_float_input() {
    let mut e = Engine::new();
    let r = e.execute("SELECT ceil(1.5)").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, spg_storage::DataType::Float);
}

// ── INTEGRATION ─────────────────────────────────────────────────

#[test]
fn ceil_for_page_count() {
    // Common pagination: ceil(total / per_page).
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT ceil(100.0 / 30.0)"), 4.0);
    assert_eq!(float_result(&mut e, "SELECT ceil(90.0 / 30.0)"), 3.0);
}

#[test]
fn ceil_inside_where() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, x FLOAT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 1.1), (2, 2.1), (3, 3.1)")
        .unwrap();
    let r = e
        .execute("SELECT id FROM u WHERE ceil(x) = 3")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(2));
}
