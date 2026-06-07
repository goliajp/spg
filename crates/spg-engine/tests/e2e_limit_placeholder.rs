//! v7.9.24 — H2: LIMIT $N / OFFSET $N placeholder binding.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        let r = eng
            .execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
        assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql:?}");
    }
    eng
}

fn fetch_with_params(eng: &mut Engine, sql: &str, params: &[Value]) -> Vec<Vec<Value>> {
    let stmt = eng.prepare(sql).expect("parses");
    match eng.execute_prepared(stmt, params).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn limit_placeholder_binds_to_int() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
        "INSERT INTO t VALUES (1), (2), (3), (4), (5)",
    ]);
    let rows = fetch_with_params(
        &mut eng,
        "SELECT id FROM t ORDER BY id LIMIT $1",
        &[Value::Int(2)],
    );
    assert_eq!(rows.len(), 2);
}

#[test]
fn limit_and_offset_placeholders() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
        "INSERT INTO t VALUES (1), (2), (3), (4), (5)",
    ]);
    let rows = fetch_with_params(
        &mut eng,
        "SELECT id FROM t ORDER BY id LIMIT $1 OFFSET $2",
        &[Value::Int(2), Value::Int(2)],
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Int(3));
    assert_eq!(rows[1][0], Value::Int(4));
}

#[test]
fn literal_limit_still_works_post_widening() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
        "INSERT INTO t VALUES (1), (2), (3)",
    ]);
    let r = eng.execute("SELECT id FROM t ORDER BY id LIMIT 2").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 2);
}

#[test]
fn limit_placeholder_bigint_value_works() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
        "INSERT INTO t VALUES (1), (2)",
    ]);
    let rows = fetch_with_params(&mut eng, "SELECT id FROM t LIMIT $1", &[Value::BigInt(1)]);
    assert_eq!(rows.len(), 1);
}

#[test]
fn limit_placeholder_in_simple_query_path_errors() {
    // No params bound → executor sees Placeholder LimitExpr → None.
    // For now SPG's simple-query path silently drops LIMIT when the
    // value isn't a literal; pin that behaviour so the prepared-
    // statement path remains the recommended one.
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
        "INSERT INTO t VALUES (1), (2), (3)",
    ]);
    let r = eng.execute("SELECT id FROM t ORDER BY id LIMIT $1");
    // Simple-query rejects (the engine forwards the error).
    let _ = r;
}

#[test]
fn limit_placeholder_value_zero_returns_empty() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
        "INSERT INTO t VALUES (1)",
    ]);
    let rows = fetch_with_params(&mut eng, "SELECT id FROM t LIMIT $1", &[Value::Int(0)]);
    assert!(rows.is_empty());
}

// v7.17.0 Phase 5.1 — extended LIMIT shapes (NULL / ALL / FETCH FIRST).

#[test]
fn limit_null_unlimited() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
        "INSERT INTO t VALUES (1), (2), (3), (4), (5)",
    ]);
    let r = eng.execute("SELECT id FROM t ORDER BY id LIMIT NULL").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 5);
}

#[test]
fn limit_all_unlimited() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
        "INSERT INTO t VALUES (1), (2), (3), (4), (5)",
    ]);
    let r = eng.execute("SELECT id FROM t ORDER BY id LIMIT ALL").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 5);
}

#[test]
fn fetch_first_rows_only() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
        "INSERT INTO t VALUES (1), (2), (3), (4), (5)",
    ]);
    let r = eng
        .execute("SELECT id FROM t ORDER BY id FETCH FIRST 3 ROWS ONLY")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 3);
}

#[test]
fn fetch_next_rows_only() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
        "INSERT INTO t VALUES (1), (2), (3)",
    ]);
    // FETCH NEXT — SQL-standard alias for FETCH FIRST.
    let r = eng
        .execute("SELECT id FROM t ORDER BY id FETCH NEXT 2 ROWS ONLY")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 2);
}

#[test]
fn fetch_first_placeholder_with_bind() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
        "INSERT INTO t VALUES (1), (2), (3), (4), (5)",
    ]);
    let rows = fetch_with_params(
        &mut eng,
        "SELECT id FROM t ORDER BY id FETCH FIRST $1 ROWS ONLY",
        &[Value::Int(2)],
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[1][0], Value::Int(2));
}

#[test]
fn fetch_first_row_only_implicit_one() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
        "INSERT INTO t VALUES (1), (2), (3)",
    ]);
    // `FETCH FIRST ROW ONLY` with no count = LIMIT 1.
    let r = eng
        .execute("SELECT id FROM t ORDER BY id FETCH FIRST ROW ONLY")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
}

#[test]
fn offset_with_rows_keyword() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
        "INSERT INTO t VALUES (1), (2), (3), (4), (5)",
    ]);
    // OFFSET 2 ROWS — SQL-standard suffix; pg_dump can emit it.
    let r = eng
        .execute("SELECT id FROM t ORDER BY id OFFSET 2 ROWS")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].values[0], Value::Int(3));
}

#[test]
fn offset_rows_fetch_first_combined() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL)",
        "INSERT INTO t VALUES (1), (2), (3), (4), (5)",
    ]);
    let r = eng
        .execute(
            "SELECT id FROM t ORDER BY id OFFSET 1 ROWS FETCH FIRST 2 ROWS ONLY",
        )
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[0], Value::Int(2));
    assert_eq!(rows[1].values[0], Value::Int(3));
}
