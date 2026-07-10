//! v7.38 (read01) — DISTINCT / UNION / INTERSECT / EXCEPT treat numerically-equal
//! exact values as one regardless of type or scale (1 = 1.0 = 1.00), matching PG
//! (and GROUP BY). Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn nrows(e: &mut Engine, sql: &str) -> usize {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => panic!("rows"),
    }
}

#[test]
fn numeric_scale_dedup() {
    let mut e = Engine::new();
    // UNION across int / numeric / scale.
    assert_eq!(nrows(&mut e, "SELECT 1 UNION SELECT 1.0"), 1);
    assert_eq!(nrows(&mut e, "SELECT 1.0 UNION SELECT 1.00"), 1);
    assert_eq!(nrows(&mut e, "SELECT -1.0 UNION SELECT -1.00"), 1);
    assert_eq!(nrows(&mut e, "SELECT 2.5 UNION SELECT 2.50"), 1);
    assert_eq!(nrows(&mut e, "SELECT 1 UNION SELECT 2"), 2);
    // INTERSECT / EXCEPT honor the same equality.
    assert_eq!(nrows(&mut e, "SELECT 1.0 INTERSECT SELECT 1.00"), 1);
    assert_eq!(nrows(&mut e, "SELECT 1.0 EXCEPT SELECT 1.00"), 0);
    // DISTINCT on a real column.
    e.execute("CREATE TABLE nd(x numeric)").unwrap();
    e.execute("INSERT INTO nd VALUES (1.0),(1.00),(1.000),(2.0),(2.5)")
        .unwrap();
    assert_eq!(nrows(&mut e, "SELECT DISTINCT x FROM nd"), 3); // {1, 2, 2.5}
    assert_eq!(nrows(&mut e, "SELECT DISTINCT x FROM nd ORDER BY x"), 3);
}
