//! v7.38 (read01 P4.14) — GENERATED ALWAYS AS (expr) VIRTUAL (PG 18) parses
//! and behaves like a generated column: the value is computed, recomputed
//! when a base column changes, and NOT NULL is enforced on the computed
//! value. (SPG computes-and-stores; observably identical to PG's VIRTUAL.)

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> Vec<spg_storage::Value<'static>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values.clone(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn virtual_generated_column_computes_and_enforces_not_null() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE g(id int, v int GENERATED ALWAYS AS (id * 2) VIRTUAL)")
        .unwrap();
    e.execute("INSERT INTO g(id) VALUES (5)").unwrap();
    assert_eq!(
        one(&mut e, "SELECT id, v FROM g"),
        vec![spg_storage::Value::Int(5), spg_storage::Value::Int(10)]
    );
    // Recomputes when the base column changes.
    e.execute("UPDATE g SET id = 10").unwrap();
    assert_eq!(
        one(&mut e, "SELECT id, v FROM g"),
        vec![spg_storage::Value::Int(10), spg_storage::Value::Int(20)]
    );

    // NOT NULL on a virtual column is checked against the computed value.
    e.execute("CREATE TABLE gn(id int, v int GENERATED ALWAYS AS (id * 2) VIRTUAL NOT NULL)")
        .unwrap();
    // v7.39 — surfaces as PG's full 23502 form (relation-qualified),
    // which the INSERT entry wraps out of the storage error.
    let err = e
        .execute("INSERT INTO gn(id) VALUES (NULL)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("violates not-null constraint") && err.contains("relation \"gn\""),
        "got: {err}"
    );
}
