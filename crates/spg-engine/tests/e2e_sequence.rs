//! v7.17.0 Phase 1.1 — CREATE / ALTER / DROP SEQUENCE + nextval /
//! currval / setval round-trip. Validates the parser + catalog
//! round-trip and the pre-resolve walker that replaces sequence
//! FunctionCall nodes with their concrete Literal values in
//! INSERT VALUES tuples.

use spg_engine::{Engine, QueryResult};

#[test]
fn create_sequence_default_starts_at_one() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE s1").unwrap();
    let r = e.execute("SELECT nextval('s1')").unwrap();
    let rows = match r {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows"),
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(value_as_i64(&rows[0].values[0]), 1);
}

fn value_as_i64(v: &spg_storage::Value) -> i64 {
    match v {
        spg_storage::Value::SmallInt(n) => i64::from(*n),
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

#[test]
fn create_sequence_with_options_round_trips() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE s START WITH 100 INCREMENT BY 5 MAXVALUE 200 CYCLE")
        .unwrap();
    // first nextval returns START.
    assert_bigint_eq(&mut e, "SELECT nextval('s')", 100);
    assert_bigint_eq(&mut e, "SELECT nextval('s')", 105);
    assert_bigint_eq(&mut e, "SELECT nextval('s')", 110);
    // currval = last handed out.
    assert_bigint_eq(&mut e, "SELECT currval('s')", 110);
}

#[test]
fn nextval_in_insert_values_per_row() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE foo_id_seq").unwrap();
    e.execute("CREATE TABLE foo (id BIGINT NOT NULL, name TEXT)")
        .unwrap();
    e.execute(
        "INSERT INTO foo (id, name) VALUES \
           (nextval('foo_id_seq'), 'a'), \
           (nextval('foo_id_seq'), 'b'), \
           (nextval('foo_id_seq'), 'c')",
    )
    .unwrap();
    let r = e.execute("SELECT id, name FROM foo").unwrap();
    let rows = match r {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows"),
    };
    assert_eq!(rows.len(), 3);
    // IDs minted per-row.
    assert_eq!(value_as_i64(&rows[0].values[0]), 1);
    assert_eq!(value_as_i64(&rows[1].values[0]), 2);
    assert_eq!(value_as_i64(&rows[2].values[0]), 3);
}

#[test]
fn setval_then_nextval_picks_up() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE s").unwrap();
    // setval(s, 99) → is_called=true so next nextval = 99+1 = 100.
    assert_bigint_eq(&mut e, "SELECT setval('s', 99)", 99);
    assert_bigint_eq(&mut e, "SELECT nextval('s')", 100);
    // setval(s, 50, false) → is_called=false so next nextval = 50.
    assert_bigint_eq(&mut e, "SELECT setval('s', 50, false)", 50);
    assert_bigint_eq(&mut e, "SELECT nextval('s')", 50);
}

#[test]
fn currval_errors_before_first_nextval() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE s").unwrap();
    let r = e.execute("SELECT currval('s')");
    assert!(r.is_err(), "currval before nextval should error");
}

#[test]
fn create_sequence_if_not_exists_noop() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE s").unwrap();
    e.execute("CREATE SEQUENCE IF NOT EXISTS s START WITH 999")
        .unwrap();
    // First create wins — START=1 (default).
    assert_bigint_eq(&mut e, "SELECT nextval('s')", 1);
}

#[test]
fn alter_sequence_restart_with() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE s").unwrap();
    assert_bigint_eq(&mut e, "SELECT nextval('s')", 1);
    assert_bigint_eq(&mut e, "SELECT nextval('s')", 2);
    e.execute("ALTER SEQUENCE s RESTART WITH 1000").unwrap();
    assert_bigint_eq(&mut e, "SELECT nextval('s')", 1000);
}

#[test]
fn drop_sequence_removes_it() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE s").unwrap();
    e.execute("DROP SEQUENCE s").unwrap();
    let r = e.execute("SELECT nextval('s')");
    assert!(r.is_err(), "nextval on dropped sequence should error");
}

#[test]
fn drop_sequence_if_exists_silent_on_missing() {
    let mut e = Engine::new();
    e.execute("DROP SEQUENCE IF EXISTS missing").unwrap();
}

#[test]
fn cycle_wraps_to_minvalue() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE s START WITH 1 MAXVALUE 3 CYCLE")
        .unwrap();
    assert_bigint_eq(&mut e, "SELECT nextval('s')", 1);
    assert_bigint_eq(&mut e, "SELECT nextval('s')", 2);
    assert_bigint_eq(&mut e, "SELECT nextval('s')", 3);
    // Wrap.
    assert_bigint_eq(&mut e, "SELECT nextval('s')", 1);
}

#[test]
fn no_cycle_errors_at_max() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE s START WITH 1 MAXVALUE 2").unwrap();
    assert_bigint_eq(&mut e, "SELECT nextval('s')", 1);
    assert_bigint_eq(&mut e, "SELECT nextval('s')", 2);
    let r = e.execute("SELECT nextval('s')");
    assert!(r.is_err(), "nextval past MAXVALUE without CYCLE should error");
}

#[test]
fn catalog_round_trip_preserves_sequence_state() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE s START WITH 100 INCREMENT BY 7")
        .unwrap();
    e.execute("SELECT nextval('s')").unwrap();
    e.execute("SELECT nextval('s')").unwrap();
    let snapshot = e.catalog().serialize();
    let restored = spg_storage::Catalog::deserialize(&snapshot).expect("round-trip");
    let seq = restored.sequences().get("s").expect("sequence persisted");
    assert_eq!(seq.start, 100);
    assert_eq!(seq.increment, 7);
    assert_eq!(seq.last_value, 107);
    assert!(seq.is_called);
}

// --- helpers ---

fn assert_bigint_eq(e: &mut Engine, sql: &str, want: i64) {
    let r = e.execute(sql).unwrap();
    let rows = match r {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows for {sql:?}"),
    };
    assert_eq!(rows.len(), 1, "row count mismatch for {sql:?}");
    let v = &rows[0].values[0];
    let got = match v {
        spg_storage::Value::SmallInt(n) => i64::from(*n),
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("not an integer for {sql:?}: {other:?}"),
    };
    assert_eq!(got, want, "{sql:?} returned {got}, want {want}");
}
