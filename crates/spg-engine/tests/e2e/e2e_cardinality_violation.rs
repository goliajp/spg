//! v7.38 (read01 P4.02) — a scalar / row subquery used as an expression
//! that returns more than one row raises PG's CARDINALITY_VIOLATION
//! (SQLSTATE 21000) with PG's message; an empty one yields NULL.

use spg_engine::{Engine, EngineError, QueryResult};

fn scalar(e: &mut Engine, sql: &str) -> QueryResult {
    e.execute(sql).unwrap()
}

#[test]
fn single_row_subquery_cardinality() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE m(v int, w int)").unwrap();
    e.execute("INSERT INTO m VALUES (1, 10), (2, 20)").unwrap();

    // More than one row → CardinalityViolation with PG's exact wording.
    let err = e.execute("SELECT (SELECT v FROM m)").unwrap_err();
    assert!(matches!(err, EngineError::CardinalityViolation));
    assert_eq!(
        err.to_string(),
        "more than one row returned by a subquery used as an expression"
    );
    // Row-comparison subquery has the same rule.
    assert!(matches!(
        e.execute("SELECT (1, 10) = (SELECT v, w FROM m)")
            .unwrap_err(),
        EngineError::CardinalityViolation
    ));

    // Empty subquery → NULL (the "empty defaults" half of the rule).
    assert!(matches!(
        scalar(&mut e, "SELECT (SELECT v FROM m WHERE v > 100)"),
        QueryResult::Rows { .. }
    ));
    // Exactly one row → the value.
    let QueryResult::Rows { rows, .. } = scalar(&mut e, "SELECT (SELECT v FROM m WHERE v = 1)")
    else {
        panic!();
    };
    assert_eq!(rows[0].values[0], spg_storage::Value::Int(1));
}
