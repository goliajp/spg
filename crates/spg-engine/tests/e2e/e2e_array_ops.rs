//! v7.11.6-8 — array ops (Epic 2 of v7.11): array_length /
//! array_position / unnest / `||` array concat.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn ok(eng: &mut Engine, sql: &str) -> QueryResult {
    eng.execute(sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"))
}

fn select_value(eng: &mut Engine, sql: &str) -> Value<'static> {
    match ok(eng, sql) {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .next()
            .map(|mut r| r.values.remove(0))
            .expect("at least one row"),
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn select_rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match ok(eng, sql) {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn array_length_dim_1() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (labels TEXT[] NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (ARRAY['a', 'b', 'c'])");
    let v = select_value(&mut eng, "SELECT array_length(labels, 1) FROM t");
    assert!(matches!(v, Value::Int(3)));
}

#[test]
fn array_length_other_dim_returns_null() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (labels TEXT[] NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (ARRAY['a', 'b'])");
    let v = select_value(&mut eng, "SELECT array_length(labels, 2) FROM t");
    assert!(matches!(v, Value::Null));
}

#[test]
fn array_position_finds_match() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (labels TEXT[] NOT NULL)");
    ok(
        &mut eng,
        "INSERT INTO t VALUES (ARRAY['alpha', 'beta', 'gamma'])",
    );
    let v = select_value(&mut eng, "SELECT array_position(labels, 'beta') FROM t");
    assert!(matches!(v, Value::Int(2)));
}

#[test]
fn array_position_returns_null_when_absent() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (labels TEXT[] NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (ARRAY['a', 'b'])");
    let v = select_value(&mut eng, "SELECT array_position(labels, 'z') FROM t");
    assert!(matches!(v, Value::Null));
}

#[test]
fn array_position_skips_null_elements() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (labels TEXT[] NOT NULL)");
    ok(
        &mut eng,
        "INSERT INTO t VALUES (ARRAY['a', NULL, 'b', 'c'])",
    );
    let v = select_value(&mut eng, "SELECT array_position(labels, 'b') FROM t");
    assert!(matches!(v, Value::Int(3)));
}

#[test]
fn array_concat_array_array() {
    let mut eng = Engine::new();
    let v = select_value(&mut eng, "SELECT ARRAY['a', 'b'] || ARRAY['c', 'd']");
    let Value::TextArray(items) = v else {
        panic!();
    };
    assert_eq!(
        items,
        vec![
            Some("a".to_string()),
            Some("b".to_string()),
            Some("c".to_string()),
            Some("d".to_string())
        ]
    );
}

#[test]
fn array_concat_array_text_appends() {
    let mut eng = Engine::new();
    let v = select_value(&mut eng, "SELECT ARRAY['a', 'b'] || 'c'");
    let Value::TextArray(items) = v else {
        panic!();
    };
    assert_eq!(
        items,
        vec![
            Some("a".to_string()),
            Some("b".to_string()),
            Some("c".to_string())
        ]
    );
}

#[test]
fn array_concat_text_array_prepends() {
    let mut eng = Engine::new();
    let v = select_value(&mut eng, "SELECT 'head' || ARRAY['mid', 'tail']");
    let Value::TextArray(items) = v else {
        panic!();
    };
    assert_eq!(
        items,
        vec![
            Some("head".to_string()),
            Some("mid".to_string()),
            Some("tail".to_string())
        ]
    );
}

#[test]
fn unnest_basic() {
    let mut eng = Engine::new();
    let rows = select_rows(
        &mut eng,
        "SELECT u FROM unnest(ARRAY['x', 'y', 'z']) u ORDER BY u",
    );
    assert_eq!(rows.len(), 3);
    assert!(matches!(rows[0][0], Value::Text(ref s) if s == "x"));
    assert!(matches!(rows[1][0], Value::Text(ref s) if s == "y"));
    assert!(matches!(rows[2][0], Value::Text(ref s) if s == "z"));
}

#[test]
fn unnest_emits_null_elements_as_null_rows() {
    let mut eng = Engine::new();
    let rows = select_rows(&mut eng, "SELECT u FROM unnest(ARRAY['a', NULL, 'b']) u");
    assert_eq!(rows.len(), 3);
    assert!(matches!(rows[0][0], Value::Text(ref s) if s == "a"));
    assert!(matches!(rows[1][0], Value::Null));
    assert!(matches!(rows[2][0], Value::Text(ref s) if s == "b"));
}

#[test]
fn unnest_with_where_filter() {
    let mut eng = Engine::new();
    let rows = select_rows(
        &mut eng,
        "SELECT u FROM unnest(ARRAY['alpha', 'beta', 'gamma', 'delta']) u WHERE u LIKE 'g%'",
    );
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Text(ref s) if s == "gamma"));
}

#[test]
fn unnest_with_limit() {
    let mut eng = Engine::new();
    let rows = select_rows(
        &mut eng,
        "SELECT u FROM unnest(ARRAY['a', 'b', 'c', 'd', 'e']) u LIMIT 2",
    );
    assert_eq!(rows.len(), 2);
}

#[test]
fn unnest_from_pg_external_form_cast() {
    let mut eng = Engine::new();
    let rows = select_rows(&mut eng, "SELECT u FROM unnest('{x,y,z}'::TEXT[]) u");
    assert_eq!(rows.len(), 3);
}
