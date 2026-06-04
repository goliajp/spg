//! v7.10.9-12 — TEXT[] end-to-end. Epic 2 of v7.10.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn ok(eng: &mut Engine, sql: &str) -> QueryResult {
    eng.execute(sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"))
}

fn select_value(eng: &mut Engine, sql: &str) -> Value {
    match ok(eng, sql) {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .next()
            .map(|mut r| r.values.remove(0))
            .expect("at least one row"),
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn select_rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Value>> {
    match ok(eng, sql) {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn text_array_column_create_insert_select() {
    let mut eng = Engine::new();
    ok(
        &mut eng,
        "CREATE TABLE messages (id INT NOT NULL, labels TEXT[] NOT NULL)",
    );
    ok(
        &mut eng,
        "INSERT INTO messages VALUES (1, ARRAY['inbox', 'work'])",
    );
    let v = select_value(&mut eng, "SELECT labels FROM messages");
    let Value::TextArray(items) = v else {
        panic!("expected TextArray");
    };
    assert_eq!(
        items,
        vec![Some("inbox".to_string()), Some("work".to_string())]
    );
}

#[test]
fn array_literal_with_null() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a TEXT[] NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (ARRAY['x', NULL, 'y'])");
    let v = select_value(&mut eng, "SELECT a FROM t");
    let Value::TextArray(items) = v else {
        panic!();
    };
    assert_eq!(
        items,
        vec![Some("x".to_string()), None, Some("y".to_string())]
    );
}

#[test]
fn pg_external_array_form_cast() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a TEXT[] NOT NULL)");
    ok(
        &mut eng,
        "INSERT INTO t VALUES ('{red,green,blue}'::TEXT[])",
    );
    let v = select_value(&mut eng, "SELECT a FROM t");
    let Value::TextArray(items) = v else {
        panic!();
    };
    assert_eq!(
        items,
        vec![
            Some("red".to_string()),
            Some("green".to_string()),
            Some("blue".to_string())
        ]
    );
}

#[test]
fn pg_external_form_with_null_and_quoted() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a TEXT[] NOT NULL)");
    ok(
        &mut eng,
        "INSERT INTO t VALUES ('{\"hello, world\",NULL,plain}'::TEXT[])",
    );
    let v = select_value(&mut eng, "SELECT a FROM t");
    let Value::TextArray(items) = v else {
        panic!();
    };
    assert_eq!(
        items,
        vec![
            Some("hello, world".to_string()),
            None,
            Some("plain".to_string())
        ]
    );
}

#[test]
fn equals_any_filters_rows() {
    let mut eng = Engine::new();
    ok(
        &mut eng,
        "CREATE TABLE t (id INT NOT NULL, labels TEXT[] NOT NULL)",
    );
    ok(&mut eng, "INSERT INTO t VALUES (1, ARRAY['a', 'b'])");
    ok(&mut eng, "INSERT INTO t VALUES (2, ARRAY['c', 'd'])");
    ok(&mut eng, "INSERT INTO t VALUES (3, ARRAY['a', 'd'])");
    let rows = select_rows(
        &mut eng,
        "SELECT id FROM t WHERE 'a' = ANY(labels) ORDER BY id",
    );
    assert_eq!(rows.len(), 2);
    assert!(matches!(rows[0][0], Value::Int(1)));
    assert!(matches!(rows[1][0], Value::Int(3)));
}

#[test]
fn not_equals_all_filters_rows() {
    let mut eng = Engine::new();
    ok(
        &mut eng,
        "CREATE TABLE t (id INT NOT NULL, labels TEXT[] NOT NULL)",
    );
    ok(&mut eng, "INSERT INTO t VALUES (1, ARRAY['a', 'b'])");
    ok(&mut eng, "INSERT INTO t VALUES (2, ARRAY['c', 'd'])");
    // <> ALL: id 2 has no 'a' or 'b' — true.
    let rows = select_rows(
        &mut eng,
        "SELECT id FROM t WHERE 'a' <> ALL(labels) ORDER BY id",
    );
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Int(2)));
}

#[test]
fn array_subscript_one_based() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a TEXT[] NOT NULL)");
    ok(
        &mut eng,
        "INSERT INTO t VALUES (ARRAY['alpha', 'beta', 'gamma'])",
    );
    let v = select_value(&mut eng, "SELECT a[2] FROM t");
    assert!(matches!(v, Value::Text(ref s) if s == "beta"), "{v:?}");
}

#[test]
fn array_subscript_out_of_range_returns_null() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a TEXT[] NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (ARRAY['only'])");
    let v = select_value(&mut eng, "SELECT a[5] FROM t");
    assert!(matches!(v, Value::Null));
    let v = select_value(&mut eng, "SELECT a[0] FROM t");
    assert!(matches!(v, Value::Null));
}

#[test]
fn text_array_persists_across_snapshot() {
    let mut eng = Engine::new();
    ok(
        &mut eng,
        "CREATE TABLE t (id INT NOT NULL, labels TEXT[] NOT NULL)",
    );
    ok(&mut eng, "INSERT INTO t VALUES (1, ARRAY['one', 'two'])");
    ok(
        &mut eng,
        "INSERT INTO t VALUES (2, ARRAY['three', NULL, 'four'])",
    );
    let bytes = eng.snapshot();
    let mut eng2 = Engine::restore_envelope(&bytes).expect("reload");
    let v = select_value(&mut eng2, "SELECT labels FROM t WHERE id = 2");
    let Value::TextArray(items) = v else { panic!() };
    assert_eq!(
        items,
        vec![Some("three".to_string()), None, Some("four".to_string())]
    );
}

#[test]
fn empty_array_literal() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a TEXT[] NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (ARRAY[]::TEXT[])");
    let v = select_value(&mut eng, "SELECT a FROM t");
    let Value::TextArray(items) = v else { panic!() };
    assert!(items.is_empty());
}
