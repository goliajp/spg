//! v6.4.2 — Window function `IGNORE NULLS` / `RESPECT NULLS`.
//!
//! LAG / LEAD with IGNORE NULLS skip NULL values when walking back/
//! forward. FIRST_VALUE / LAST_VALUE with IGNORE NULLS pick the
//! first / last non-NULL within the frame. Default (no modifier) is
//! RESPECT — current behaviour.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(res: QueryResult) -> Vec<Vec<Value>> {
    match res {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn lag_ignore_nulls_skips_nulls() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    // Rows ordered by id; v has NULLs interleaved.
    // id=1 v=10, id=2 v=NULL, id=3 v=20, id=4 v=NULL, id=5 v=30
    eng.execute("INSERT INTO t VALUES (1, 10)").unwrap();
    eng.execute("INSERT INTO t VALUES (2, NULL)").unwrap();
    eng.execute("INSERT INTO t VALUES (3, 20)").unwrap();
    eng.execute("INSERT INTO t VALUES (4, NULL)").unwrap();
    eng.execute("INSERT INTO t VALUES (5, 30)").unwrap();

    let res = eng
        .execute(
            "SELECT id, LAG(v) IGNORE NULLS OVER (ORDER BY id) FROM t ORDER BY id",
        )
        .unwrap();
    let got = rows_of(res);
    // Expected LAG(v) IGNORE NULLS:
    //   id=1 → NULL (no prior)
    //   id=2 → 10   (prev non-NULL)
    //   id=3 → 10   (prev non-NULL, skipping nothing)
    //   id=4 → 20
    //   id=5 → 20   (skip id=4's NULL, take id=3's 20)
    assert_eq!(
        got,
        vec![
            vec![Value::Int(1), Value::Null],
            vec![Value::Int(2), Value::Int(10)],
            vec![Value::Int(3), Value::Int(10)],
            vec![Value::Int(4), Value::Int(20)],
            vec![Value::Int(5), Value::Int(20)],
        ]
    );
}

#[test]
fn first_value_respect_nulls_default() {
    // Without IGNORE NULLS, FIRST_VALUE returns the literal first
    // value in the frame, even if it's NULL.
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    eng.execute("INSERT INTO t VALUES (1, NULL)").unwrap();
    eng.execute("INSERT INTO t VALUES (2, 20)").unwrap();
    eng.execute("INSERT INTO t VALUES (3, 30)").unwrap();

    let res = eng
        .execute(
            "SELECT id, FIRST_VALUE(v) OVER (ORDER BY id) FROM t ORDER BY id",
        )
        .unwrap();
    let got = rows_of(res);
    // First value in the running frame is always row 1's v = NULL.
    assert_eq!(got[0][1], Value::Null);
    assert_eq!(got[1][1], Value::Null);
    assert_eq!(got[2][1], Value::Null);
}

#[test]
fn first_value_ignore_nulls_picks_first_non_null() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    eng.execute("INSERT INTO t VALUES (1, NULL)").unwrap();
    eng.execute("INSERT INTO t VALUES (2, 20)").unwrap();
    eng.execute("INSERT INTO t VALUES (3, 30)").unwrap();

    let res = eng
        .execute(
            "SELECT id, FIRST_VALUE(v) IGNORE NULLS OVER (ORDER BY id) FROM t ORDER BY id",
        )
        .unwrap();
    let got = rows_of(res);
    // FIRST_VALUE IGNORE NULLS in a running frame (start..current):
    //   id=1 → NULL (no non-NULL in frame [row 1])
    //   id=2 → 20   (first non-NULL in [row 1, row 2])
    //   id=3 → 20   (still 20)
    assert_eq!(got[0][1], Value::Null);
    assert_eq!(got[1][1], Value::Int(20));
    assert_eq!(got[2][1], Value::Int(20));
}

#[test]
fn last_value_ignore_nulls() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    eng.execute("INSERT INTO t VALUES (1, 10)").unwrap();
    eng.execute("INSERT INTO t VALUES (2, 20)").unwrap();
    eng.execute("INSERT INTO t VALUES (3, NULL)").unwrap();
    eng.execute("INSERT INTO t VALUES (4, NULL)").unwrap();

    let res = eng
        .execute(
            "SELECT id, LAST_VALUE(v) IGNORE NULLS OVER (ORDER BY id) FROM t ORDER BY id",
        )
        .unwrap();
    let got = rows_of(res);
    // Running frame [row 1..current]; last non-NULL in each window:
    //   id=1 → 10
    //   id=2 → 20
    //   id=3 → 20 (skip NULL)
    //   id=4 → 20
    assert_eq!(got[0][1], Value::Int(10));
    assert_eq!(got[1][1], Value::Int(20));
    assert_eq!(got[2][1], Value::Int(20));
    assert_eq!(got[3][1], Value::Int(20));
}
