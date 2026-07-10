//! v7.38 (read01, U16) — ORDER BY on a one-dimensional array sorts element-wise,
//! then shorter-first, with integer arrays comparing numerically (not by text):
//! {1} < {1,2} < {1,5} < {2} < {10}. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn col0(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::IntArray(a) => format!("{a:?}"),
                spg_storage::Value::TextArray(a) => format!("{a:?}"),
                v => format!("{v:?}"),
            })
            .collect(),
        _ => panic!("rows"),
    }
}

#[test]
fn order_by_array() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE arr(a int[])").unwrap();
    e.execute("INSERT INTO arr VALUES (ARRAY[2]),(ARRAY[10]),(ARRAY[1,5]),(ARRAY[1,2]),(ARRAY[1])")
        .unwrap();
    // Numeric element-wise, shorter-first — NOT text order (which would put
    // {10} before {2}).
    assert_eq!(
        col0(&mut e, "SELECT a FROM arr ORDER BY a"),
        vec![
            "[Some(1)]",
            "[Some(1), Some(2)]",
            "[Some(1), Some(5)]",
            "[Some(2)]",
            "[Some(10)]",
        ]
    );
    // DESC reverses.
    assert_eq!(
        col0(&mut e, "SELECT a FROM arr ORDER BY a DESC")[0],
        "[Some(10)]"
    );

    e.execute("CREATE TABLE tarr(t text[])").unwrap();
    e.execute("INSERT INTO tarr VALUES (ARRAY['b']),(ARRAY['a','z']),(ARRAY['a'])")
        .unwrap();
    assert_eq!(
        col0(&mut e, "SELECT t FROM tarr ORDER BY t"),
        vec![
            "[Some(\"a\")]",
            "[Some(\"a\"), Some(\"z\")]",
            "[Some(\"b\")]"
        ]
    );
}
