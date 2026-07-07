//! v7.38 (read01, T4) — sum(bigint) / avg(bigint) widen to NUMERIC (PG), which
//! also defends the sum against i64 overflow. sum(int) stays bigint and
//! avg(int) stays double. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn sum_avg_bigint_widen_to_numeric() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (i int, b bigint)").unwrap();
    e.execute("INSERT INTO t VALUES (1, 1000000000000), (2, 2000000000000), (3, 3000000000000)")
        .unwrap();

    // sum(int) stays bigint; sum(bigint) -> numeric.
    assert_eq!(text(&mut e, "SELECT pg_typeof(sum(i)) FROM t"), "bigint");
    assert_eq!(text(&mut e, "SELECT pg_typeof(sum(b)) FROM t"), "numeric");
    assert_eq!(text(&mut e, "SELECT sum(b)::text FROM t"), "6000000000000");

    // avg(int) stays double; avg(bigint) -> numeric.
    assert_eq!(text(&mut e, "SELECT pg_typeof(avg(i)) FROM t"), "double precision");
    assert_eq!(text(&mut e, "SELECT pg_typeof(avg(b)) FROM t"), "numeric");

    // Overflow safety: two values near i64::MAX sum past 9.2e18 without wrapping.
    e.execute("CREATE TABLE big (v bigint)").unwrap();
    e.execute("INSERT INTO big VALUES (9000000000000000000), (9000000000000000000)")
        .unwrap();
    assert_eq!(text(&mut e, "SELECT sum(v)::text FROM big"), "18000000000000000000");
}
