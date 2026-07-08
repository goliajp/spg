//! v7.38 (read01, T15) — jsonb/json_object_keys is set-returning: one row per
//! top-level key (SPG previously returned a scalar TextArray; PG emits rows).
//! Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn key_rows(e: &mut Engine, sql: &str) -> Vec<String> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter()
        .map(|row| match &row.values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => panic!("expected Text, got {other:?}"),
        })
        .collect()
}

#[test]
fn jsonb_object_keys_returns_keys_in_order() {
    let mut e = Engine::new();
    assert_eq!(
        key_rows(&mut e, "SELECT jsonb_object_keys('{\"a\":1,\"b\":2,\"c\":3}'::jsonb)"),
        ["a", "b", "c"]
    );
}

#[test]
fn jsonb_object_keys_empty_object_no_rows() {
    let mut e = Engine::new();
    assert!(key_rows(&mut e, "SELECT jsonb_object_keys('{}'::jsonb)").is_empty());
}

#[test]
fn jsonb_object_keys_errors_on_non_object() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT jsonb_object_keys('[1,2,3]'::jsonb)").is_err());
    assert!(e.execute("SELECT jsonb_object_keys('42'::jsonb)").is_err());
}

#[test]
fn jsonb_object_keys_null_returns_no_rows() {
    // A NULL argument yields zero rows (PG), not one NULL row.
    let mut e = Engine::new();
    assert!(key_rows(&mut e, "SELECT jsonb_object_keys(NULL::jsonb)").is_empty());
}

#[test]
fn json_object_keys_synonym_works() {
    let mut e = Engine::new();
    assert_eq!(
        key_rows(&mut e, "SELECT json_object_keys('{\"x\":1}'::jsonb)"),
        ["x"]
    );
}

#[test]
fn jsonb_object_keys_over_table_expands_per_row() {
    // Over a real table's rows, keys expand per source row (SELECT-list SRF).
    let mut e = Engine::new();
    e.execute("CREATE TABLE ok(j jsonb)").unwrap();
    e.execute("INSERT INTO ok VALUES ('{\"a\":1}'), ('{\"b\":2,\"c\":3}')")
        .unwrap();
    assert_eq!(
        key_rows(&mut e, "SELECT jsonb_object_keys(j) FROM ok"),
        ["a", "b", "c"]
    );
}
