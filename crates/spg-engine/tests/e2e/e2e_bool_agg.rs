//! PG `bool_and` / `bool_or` / `every`.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-aggregate.html
//!
//! Invariants pinned:
//!   * `bool_and(p)`: TRUE iff every non-NULL input is TRUE.
//!   * `bool_or(p)`:  TRUE iff any non-NULL input is TRUE.
//!   * `every(p)`: standard-SQL alias for bool_and.
//!   * NULL inputs skipped.
//!   * Empty group / all-NULL → NULL (PG semantics).
//!
//! Django's Postgres `BoolAnd` / `BoolOr` aggregates compile to
//! these directly; Rails' `where(...).pluck(:...).all?` rewrites
//! via Arel use them too.

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

fn engine_with_flags() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, ok BOOL)")
        .unwrap();
    e.execute(
        "INSERT INTO u VALUES \
         (1, true),  (1, true),  (1, true), \
         (2, true),  (2, false), (2, true), \
         (3, NULL),  (3, true), \
         (4, NULL),  (4, NULL), \
         (5, false), (5, false)",
    )
    .unwrap();
    e
}

// ── bool_and ─────────────────────────────────────────────────────

#[test]
fn bool_and_all_true_group() {
    let mut e = engine_with_flags();
    let row = one_row(
        e.execute("SELECT bool_and(ok) FROM u WHERE id = 1")
            .unwrap(),
    );
    assert_eq!(row[0], Value::Bool(true));
}

#[test]
fn bool_and_with_one_false_returns_false() {
    let mut e = engine_with_flags();
    let row = one_row(
        e.execute("SELECT bool_and(ok) FROM u WHERE id = 2")
            .unwrap(),
    );
    assert_eq!(row[0], Value::Bool(false));
}

#[test]
fn bool_and_nulls_skipped() {
    // id=3: (NULL, true) → bool_and = true (NULL skipped).
    let mut e = engine_with_flags();
    let row = one_row(
        e.execute("SELECT bool_and(ok) FROM u WHERE id = 3")
            .unwrap(),
    );
    assert_eq!(row[0], Value::Bool(true));
}

#[test]
fn bool_and_all_null_returns_null() {
    let mut e = engine_with_flags();
    let row = one_row(
        e.execute("SELECT bool_and(ok) FROM u WHERE id = 4")
            .unwrap(),
    );
    assert_eq!(row[0], Value::Null);
}

#[test]
fn bool_and_empty_group_returns_null() {
    let mut e = engine_with_flags();
    let row = one_row(
        e.execute("SELECT bool_and(ok) FROM u WHERE id = 99")
            .unwrap(),
    );
    assert_eq!(row[0], Value::Null);
}

#[test]
fn bool_and_all_false_group_returns_false() {
    let mut e = engine_with_flags();
    let row = one_row(
        e.execute("SELECT bool_and(ok) FROM u WHERE id = 5")
            .unwrap(),
    );
    assert_eq!(row[0], Value::Bool(false));
}

// ── bool_or ──────────────────────────────────────────────────────

#[test]
fn bool_or_any_true_returns_true() {
    let mut e = engine_with_flags();
    let row = one_row(e.execute("SELECT bool_or(ok) FROM u WHERE id = 2").unwrap());
    assert_eq!(row[0], Value::Bool(true));
}

#[test]
fn bool_or_all_false_returns_false() {
    let mut e = engine_with_flags();
    let row = one_row(e.execute("SELECT bool_or(ok) FROM u WHERE id = 5").unwrap());
    assert_eq!(row[0], Value::Bool(false));
}

#[test]
fn bool_or_all_null_returns_null() {
    let mut e = engine_with_flags();
    let row = one_row(e.execute("SELECT bool_or(ok) FROM u WHERE id = 4").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn bool_or_empty_group_returns_null() {
    let mut e = engine_with_flags();
    let row = one_row(
        e.execute("SELECT bool_or(ok) FROM u WHERE id = 99")
            .unwrap(),
    );
    assert_eq!(row[0], Value::Null);
}

// ── every (SQL-standard alias for bool_and) ──────────────────────

#[test]
fn every_alias_returns_same_as_bool_and() {
    let mut e = engine_with_flags();
    let a = one_row(e.execute("SELECT every(ok) FROM u WHERE id = 2").unwrap());
    let b = one_row(
        e.execute("SELECT bool_and(ok) FROM u WHERE id = 2")
            .unwrap(),
    );
    assert_eq!(a, b);
    assert_eq!(a[0], Value::Bool(false));
}

// ── GROUP BY rollup ──────────────────────────────────────────────

#[test]
fn bool_and_bool_or_with_group_by() {
    let mut e = engine_with_flags();
    let r = e
        .execute(
            "SELECT id, bool_and(ok), bool_or(ok) FROM u \
             GROUP BY id ORDER BY id",
        )
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 5);
    // (1, true, true): all true
    assert_eq!(rows[0].values[1], Value::Bool(true));
    assert_eq!(rows[0].values[2], Value::Bool(true));
    // (2, false, true): mixed
    assert_eq!(rows[1].values[1], Value::Bool(false));
    assert_eq!(rows[1].values[2], Value::Bool(true));
    // (3, true, true): NULL skipped
    assert_eq!(rows[2].values[1], Value::Bool(true));
    assert_eq!(rows[2].values[2], Value::Bool(true));
    // (4, NULL, NULL): all NULL
    assert_eq!(rows[3].values[1], Value::Null);
    assert_eq!(rows[3].values[2], Value::Null);
    // (5, false, false): all false
    assert_eq!(rows[4].values[1], Value::Bool(false));
    assert_eq!(rows[4].values[2], Value::Bool(false));
}

// ── Arity errors ─────────────────────────────────────────────────

#[test]
fn bool_and_arity_errors() {
    let mut e = engine_with_flags();
    assert!(e.execute("SELECT bool_and() FROM u").is_err());
    assert!(e.execute("SELECT bool_and(ok, ok) FROM u").is_err());
}

#[test]
fn bool_or_arity_errors() {
    let mut e = engine_with_flags();
    assert!(e.execute("SELECT bool_or() FROM u").is_err());
    assert!(e.execute("SELECT bool_or(ok, ok) FROM u").is_err());
}

// ── Non-bool input errors ────────────────────────────────────────

#[test]
fn bool_and_non_bool_input_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE n (v INT)").unwrap();
    e.execute("INSERT INTO n VALUES (1), (2)").unwrap();
    assert!(e.execute("SELECT bool_and(v) FROM n").is_err());
}
