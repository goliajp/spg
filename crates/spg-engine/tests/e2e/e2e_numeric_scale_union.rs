//! v7.38 (read01) — an unconstrained NUMERIC result column (from VALUES / UNION)
//! keeps each value's own scale (PG renders `1.0` / `1.00`, not `1.00` / `1.00`),
//! while DISTINCT / aggregate-DISTINCT still collapse numerically-equal values.
//! Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn render(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Numeric { scaled, scale } => {
                    if *scale == 0 {
                        scaled.to_string()
                    } else {
                        let neg = *scaled < 0;
                        let digits = scaled.unsigned_abs().to_string();
                        let digits = format!("{:0>width$}", digits, width = *scale as usize + 1);
                        let point = digits.len() - *scale as usize;
                        format!("{}{}.{}", if neg { "-" } else { "" }, &digits[..point], &digits[point..])
                    }
                }
                v => format!("{v:?}"),
            })
            .collect(),
        _ => panic!("rows"),
    }
}

#[test]
fn numeric_scale_preserved_but_dedup_by_value() {
    let mut e = Engine::new();
    // VALUES keeps each literal's own scale.
    assert_eq!(
        render(&mut e, "SELECT x FROM (VALUES (1.0),(1.00),(2.5)) v(x) ORDER BY x"),
        vec!["1.0", "1.00", "2.5"]
    );
    // int ∪ numeric: the int promotes at scale 0, the numeric keeps its scale.
    assert_eq!(
        render(&mut e, "SELECT x FROM (SELECT 1 AS x UNION ALL SELECT 1.5) t ORDER BY x"),
        vec!["1", "1.5"]
    );
    // DISTINCT / aggregate-DISTINCT still collapse by numeric value.
    let cnt = |e: &mut Engine, sql: &str| match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            spg_storage::Value::BigInt(n) => n,
            _ => panic!(),
        },
        _ => panic!(),
    };
    assert_eq!(cnt(&mut e, "SELECT count(DISTINCT x) FROM (VALUES (1.0),(1.00),(2.5)) v(x)"), 2);
}
