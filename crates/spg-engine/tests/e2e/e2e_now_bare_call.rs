//! `now()` / `current_timestamp` / `current_date` —
//! foundational time-of-now functions every app uses. Both
//! the bare-function-call shape (`SELECT now()`) and the
//! PG keyword shape (`SELECT CURRENT_TIMESTAMP`) must work
//! identically and return the engine's current wall-clock.
//!
//! Pre-this-fix: keyword path works (parser rewrites into a
//! literal); bare call path falls through to the function
//! dispatcher which had no `now` arm and returned
//! `unknown function `now``.

use spg_engine::{Engine, QueryResult};
use spg_storage::{DataType, Value};

fn one_row(r: QueryResult) -> Vec<Value<'static>> {
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "expected exactly 1 row");
            rows.into_iter().next().unwrap().values
        }
        _ => panic!("expected rows"),
    }
}

fn engine_with_fixed_clock(value: i64) -> Engine {
    fn make(v: i64) -> impl Fn() -> i64 {
        move || v
    }
    Engine::new().with_clock_fn_static(value)
}

// Helper trait — small static-clock helper for these tests.
// (Defined in test module to keep the engine API surface clean.)
trait FixedClock {
    fn with_clock_fn_static(self, micros: i64) -> Engine;
}

impl FixedClock for Engine {
    fn with_clock_fn_static(self, micros: i64) -> Engine {
        match micros {
            1_700_000_000_000_000 => self.with_clock(|| 1_700_000_000_000_000),
            _ => panic!("add clock value to FixedClock impl: {micros}"),
        }
    }
}

const FIXED_MICROS: i64 = 1_700_000_000_000_000;

// ── BARE FUNCTION CALL ───────────────────────────────────────────

#[test]
fn now_bare_call_returns_timestamp() {
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    let row = one_row(e.execute("SELECT now()").unwrap());
    assert_eq!(row[0], Value::Timestamp(FIXED_MICROS));
}

#[test]
fn current_timestamp_bare_call_returns_timestamp() {
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    let row = one_row(e.execute("SELECT current_timestamp()").unwrap());
    assert_eq!(row[0], Value::Timestamp(FIXED_MICROS));
}

#[test]
fn current_date_bare_call_returns_date() {
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    let row = one_row(e.execute("SELECT current_date()").unwrap());
    let days = FIXED_MICROS / 86_400_000_000;
    assert_eq!(row[0], Value::Date(days as i32));
}

// ── KEYWORD FORMS (pre-existing — must still work) ───────────────

#[test]
fn current_timestamp_keyword_unchanged() {
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    let row = one_row(e.execute("SELECT CURRENT_TIMESTAMP").unwrap());
    assert_eq!(row[0], Value::Timestamp(FIXED_MICROS));
}

#[test]
fn current_date_keyword_unchanged() {
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    let row = one_row(e.execute("SELECT CURRENT_DATE").unwrap());
    let days = FIXED_MICROS / 86_400_000_000;
    assert_eq!(row[0], Value::Date(days as i32));
}

// ── EQUIVALENCE ──────────────────────────────────────────────────

#[test]
fn now_call_and_keyword_return_same_value() {
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    let a = one_row(e.execute("SELECT now()").unwrap());
    let b = one_row(e.execute("SELECT CURRENT_TIMESTAMP").unwrap());
    assert_eq!(a, b);
}

#[test]
fn current_date_call_and_keyword_return_same_value() {
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    let a = one_row(e.execute("SELECT current_date()").unwrap());
    let b = one_row(e.execute("SELECT CURRENT_DATE").unwrap());
    assert_eq!(a, b);
}

// ── CASE INSENSITIVITY ──────────────────────────────────────────

#[test]
fn now_uppercase_works() {
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    let row = one_row(e.execute("SELECT NOW()").unwrap());
    assert_eq!(row[0], Value::Timestamp(FIXED_MICROS));
}

#[test]
fn now_mixedcase_works() {
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    let row = one_row(e.execute("SELECT NoW()").unwrap());
    assert_eq!(row[0], Value::Timestamp(FIXED_MICROS));
}

// ── EXTRA ARGS REJECTED ──────────────────────────────────────────

#[test]
fn now_with_arg_errors() {
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    let r = e.execute("SELECT now('extra')");
    assert!(r.is_err(), "now() takes 0 args");
}

// ── RETURN TYPE ──────────────────────────────────────────────────

#[test]
fn now_bare_call_column_type_is_timestamp() {
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    let r = e.execute("SELECT now()").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    assert!(
        matches!(columns[0].ty, DataType::Timestamp | DataType::Timestamptz),
        "expected Timestamp/Timestamptz, got {:?}",
        columns[0].ty
    );
}

#[test]
fn current_date_bare_call_column_type_is_date() {
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    let r = e.execute("SELECT current_date()").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, DataType::Date);
}

// ── INSIDE INSERT VALUES ─────────────────────────────────────────

#[test]
fn now_inside_insert_values() {
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    e.execute("CREATE TABLE evt (created_at TIMESTAMP NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO evt VALUES (now())").unwrap();
    let row = one_row(e.execute("SELECT created_at FROM evt").unwrap());
    assert_eq!(row[0], Value::Timestamp(FIXED_MICROS));
}

#[test]
fn current_timestamp_inside_insert_values() {
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    e.execute("CREATE TABLE evt (created_at TIMESTAMP NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO evt VALUES (CURRENT_TIMESTAMP)")
        .unwrap();
    let row = one_row(e.execute("SELECT created_at FROM evt").unwrap());
    assert_eq!(row[0], Value::Timestamp(FIXED_MICROS));
}

#[test]
fn current_date_inside_insert_values() {
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    e.execute("CREATE TABLE evt (day DATE NOT NULL)").unwrap();
    e.execute("INSERT INTO evt VALUES (current_date())")
        .unwrap();
    let row = one_row(e.execute("SELECT day FROM evt").unwrap());
    let days = FIXED_MICROS / 86_400_000_000;
    assert_eq!(row[0], Value::Date(days as i32));
}

// ── INSIDE WHERE / SELECT EXPR ───────────────────────────────────

#[test]
fn now_inside_where_clause() {
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    e.execute("CREATE TABLE evt (id INT NOT NULL, t TIMESTAMP NOT NULL)")
        .unwrap();
    e.execute(&format!(
        "INSERT INTO evt VALUES (1, '{}'::TIMESTAMP)",
        "2030-01-01 00:00:00"
    ))
    .unwrap();
    // Future timestamp > now() — should select.
    let r = e.execute("SELECT id FROM evt WHERE t > now()").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
}

#[test]
fn arithmetic_with_now() {
    // now() - INTERVAL works (this is a high-frequency mailrs /
    // Django shape: WHERE created > NOW() - INTERVAL '30 days')
    let mut e = engine_with_fixed_clock(FIXED_MICROS);
    let r = e.execute("SELECT now() - '1 day'::INTERVAL").unwrap();
    let row = one_row(r);
    // FIXED_MICROS - 1 day in micros = subtract 86_400_000_000.
    let expected = FIXED_MICROS - 86_400_000_000;
    match &row[0] {
        Value::Timestamp(t) => assert_eq!(*t, expected),
        other => panic!("expected Timestamp, got {other:?}"),
    }
}
