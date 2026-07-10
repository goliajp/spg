//! v7.38 (read01) — generate_subscripts(arr, dim) is a set-returning function in
//! the SELECT list (with or without FROM), yielding the 1-based subscripts;
//! previously the SELECT-list form returned a single array row. A non-1
//! dimension over a 1-D array yields no rows, as in PG. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn ints(e: &mut Engine, sql: &str) -> Vec<i32> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match r.values.last().unwrap() {
                spg_storage::Value::Int(n) => *n,
                v => panic!("not int: {v:?}"),
            })
            .collect(),
        _ => panic!("rows"),
    }
}

#[test]
fn generate_subscripts_is_srf() {
    let mut e = Engine::new();
    // No-FROM projection form: 3 rows, not one array.
    assert_eq!(
        ints(&mut e, "SELECT generate_subscripts(ARRAY[10,20,30], 1)"),
        vec![1, 2, 3]
    );
    // Invalid dimension → no rows.
    assert_eq!(
        ints(&mut e, "SELECT generate_subscripts(ARRAY[10,20,30], 2)").len(),
        0
    );
    // Sibling scalar column repeats per subscript row.
    assert_eq!(
        ints(&mut e, "SELECT 'x', generate_subscripts(ARRAY[10,20], 1)"),
        vec![1, 2]
    );
    // Over a real FROM column.
    e.execute("CREATE TABLE t(a int[])").unwrap();
    e.execute("INSERT INTO t VALUES (ARRAY[7,8,9])").unwrap();
    assert_eq!(
        ints(&mut e, "SELECT generate_subscripts(a, 1) FROM t"),
        vec![1, 2, 3]
    );
    // FROM-position form is unchanged.
    assert_eq!(
        ints(
            &mut e,
            "SELECT s FROM generate_subscripts(ARRAY[10,20,30], 1) s"
        ),
        vec![1, 2, 3]
    );
}
