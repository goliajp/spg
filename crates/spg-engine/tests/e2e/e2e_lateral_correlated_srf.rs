//! v7.38 (read01, T-lateral) — a LATERAL set-returning function may reference a
//! preceding FROM item by a BARE (unqualified) column, not only a qualified
//! one: `t, LATERAL generate_series(1, n)`. Any column in an SRF's arguments is
//! an outer correlation (SRFs have no input columns). Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn count(e: &mut Engine, sql: &str) -> i64 {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            Value::BigInt(n) => n,
            Value::Int(n) => i64::from(n),
            ref v => panic!("expected int, got {v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn lateral_generate_series_bare_outer_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int, n int)").unwrap();
    e.execute("INSERT INTO t VALUES (1,2),(2,3)").unwrap();
    // bare `n` and qualified `t.n` both correlate; n=2 → 2 rows, n=3 → 3 rows.
    assert_eq!(count(&mut e, "SELECT count(*) FROM t, LATERAL generate_series(1, n) g"), 5);
    assert_eq!(count(&mut e, "SELECT count(*) FROM t, LATERAL generate_series(1, t.n) g"), 5);
    // Constant-arg LATERAL still cross-joins.
    assert_eq!(count(&mut e, "SELECT count(*) FROM t, LATERAL generate_series(1, 3) g"), 6);
    // SRF referencing a preceding SRF's output.
    assert_eq!(
        count(&mut e, "SELECT count(*) FROM generate_series(1,3) g1, LATERAL generate_series(1, g1) g2"),
        6
    );
}

#[test]
fn lateral_unnest_bare_outer_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int, arr int[])").unwrap();
    e.execute("INSERT INTO t VALUES (1, ARRAY[10,20]),(2, ARRAY[30])").unwrap();
    assert_eq!(count(&mut e, "SELECT count(*) FROM t, LATERAL unnest(arr) u"), 3);
}
