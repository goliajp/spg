//! v7.38 (read01, T-srf) — FROM string_to_table(s, d) / regexp_split_to_table
//! (s, p) as set-returning sources, rewritten to unnest(string_to_array(...)) /
//! unnest(regexp_split_to_array(...)). Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                Value::Text(s) => s.to_string(),
                v => format!("{v:?}"),
            })
            .collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn string_to_table_from() {
    let mut e = Engine::new();
    assert_eq!(rows(&mut e, "SELECT * FROM string_to_table('a|b|c', '|') g"), ["a", "b", "c"]);
    assert_eq!(rows(&mut e, "SELECT * FROM regexp_split_to_table('a1b2c', '[0-9]') g"), ["a", "b", "c"]);
    assert_eq!(rows(&mut e, "SELECT * FROM string_to_table('x,y,z,w', ',') g").len(), 4);
}

#[test]
fn string_to_table_projection_and_lateral() {
    let mut e = Engine::new();
    // No-FROM projection form.
    assert_eq!(rows(&mut e, "SELECT string_to_table('a|b|c', '|')"), ["a", "b", "c"]);
    assert_eq!(rows(&mut e, "SELECT regexp_split_to_table('a1b2c', '[0-9]')"), ["a", "b", "c"]);
    // Correlated LATERAL on a bare / qualified outer column.
    e.execute("CREATE TABLE t(s text)").unwrap();
    e.execute("INSERT INTO t VALUES ('x|y'),('z')").unwrap();
    use spg_engine::QueryResult;
    let n = |e: &mut Engine, sql: &str| -> i64 {
        match e.execute(sql).unwrap() {
            QueryResult::Rows { rows, .. } => match rows[0].values[0] {
                Value::BigInt(v) => v,
                Value::Int(v) => i64::from(v),
                ref v => panic!("{v:?}"),
            },
            _ => panic!(),
        }
    };
    assert_eq!(n(&mut e, "SELECT count(*) FROM t, LATERAL string_to_table(s, '|') g"), 3);
    assert_eq!(n(&mut e, "SELECT count(*) FROM t, string_to_table(t.s, '|') g"), 3);
}
