//! v7.38 (read01, T4.4) — bit_and / bit_or / bit_xor return the INPUT integer
//! type (PG: bit_and(int) → integer, bit_and(bigint) → bigint), not always
//! bigint. Oracle: live PG 18.4.

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
fn bit_aggregates_keep_input_int_type() {
    let mut e = Engine::new();
    let v = "FROM (VALUES(12),(10)) t(x)";
    assert_eq!(text(&mut e, &format!("SELECT pg_typeof(bit_and(x)) {v}")), "integer");
    assert_eq!(text(&mut e, &format!("SELECT (bit_and(x))::text {v}")), "8");
    assert_eq!(text(&mut e, &format!("SELECT pg_typeof(bit_or(x)) {v}")), "integer");
    assert_eq!(text(&mut e, &format!("SELECT pg_typeof(bit_xor(x)) {v}")), "integer");
    // bigint input → bigint result.
    assert_eq!(text(&mut e, &format!("SELECT pg_typeof(bit_or(x::bigint)) {v}")), "bigint");
}
