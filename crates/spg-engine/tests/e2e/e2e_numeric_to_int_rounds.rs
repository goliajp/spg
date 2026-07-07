//! v7.38 (read01 sweep) — coercing a NUMERIC into an integer column on INSERT
//! rounds half away from zero (PG assignment cast), matching the `::int` cast
//! path; it previously truncated. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn ints(e: &mut Engine, sql: &str) -> Vec<i64> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Int(n) => i64::from(*n),
                spg_storage::Value::BigInt(n) => *n,
                spg_storage::Value::SmallInt(n) => i64::from(*n),
                v => panic!("expected int, got {v:?}"),
            })
            .collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn numeric_into_int_column_rounds_half_away() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ti (i int)").unwrap();
    for v in ["1.7", "2.5", "1.4", "-1.5", "-2.5", "0.5"] {
        e.execute(&format!("INSERT INTO ti VALUES ({v}::numeric)"))
            .unwrap();
    }
    // 1.7→2, 2.5→3, 1.4→1, -1.5→-2, -2.5→-3, 0.5→1
    assert_eq!(ints(&mut e, "SELECT i FROM ti ORDER BY i"), vec![-3, -2, 1, 1, 2, 3]);

    // bigint + smallint columns round the same way.
    e.execute("CREATE TABLE tb (b bigint, s smallint)").unwrap();
    e.execute("INSERT INTO tb VALUES (2.5::numeric, 2.5::numeric)")
        .unwrap();
    assert_eq!(ints(&mut e, "SELECT b FROM tb"), vec![3]);
    assert_eq!(ints(&mut e, "SELECT s FROM tb"), vec![3]);

    // The INSERT coercion now agrees with the explicit `::int` cast.
    assert_eq!(ints(&mut e, "SELECT 2.5::numeric::int"), vec![3]);
}
