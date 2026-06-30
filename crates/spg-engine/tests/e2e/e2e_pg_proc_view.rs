//! v7.17.0 Phase 3.P0-51 — `pg_catalog.pg_proc` virtual view.
//!
//! ORMs (sqlx, SQLAlchemy, Diesel) and admin tools (pgAdmin,
//! DataGrip) probe pg_proc to introspect available functions.
//! SPG synthesises rows for every built-in scalar / aggregate /
//! window function the engine dispatches.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn pg_proc_lists_now() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT proname FROM pg_catalog.pg_proc WHERE proname = 'now'")
            .unwrap(),
    );
    assert!(!r.is_empty());
    assert_eq!(r[0][0], Value::text("now"));
}

#[test]
fn pg_proc_count_aggregate_is_aggregate_kind() {
    // v7.37.24 (24.6) — every count() entry (0-arg + variadic)
    // is an aggregate; check that ALL rows for proname='count'
    // carry prokind='a'.
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT prokind FROM pg_catalog.pg_proc WHERE proname = 'count'")
            .unwrap(),
    );
    assert!(!r.is_empty(), "expected at least one count() row");
    for row in &r {
        assert_eq!(row[0], Value::text("a"), "count's prokind must be 'a'");
    }
}

#[test]
fn pg_proc_window_function_kind() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT prokind FROM pg_catalog.pg_proc WHERE proname = 'row_number'")
            .unwrap(),
    );
    assert!(!r.is_empty());
    assert_eq!(r[0][0], Value::text("w"));
}

#[test]
fn pg_proc_joins_with_pg_type_on_return() {
    // The ORM-shaped query: "what does `length` return?"
    let mut e = Engine::new();
    let r = rows(
        e.execute(
            "SELECT p.proname, t.typname \
             FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_type t ON p.prorettype = t.oid \
             WHERE p.proname = 'length'",
        )
        .unwrap(),
    );
    assert!(!r.is_empty());
    assert_eq!(r[0][0], Value::text("length"));
    assert_eq!(r[0][1], Value::text("int4"));
}
