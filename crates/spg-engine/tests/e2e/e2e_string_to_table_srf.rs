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
