//! v7.38 (read01) — numeric literals accept `_` digit separators between
//! digits (PG 16+): 1_000, 1_000_000, 1_000.5, 1_0e3. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Int(n) => n.to_string(),
            spg_storage::Value::BigInt(n) => n.to_string(),
            spg_storage::Value::Float(x) => x.to_string(),
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn underscore_digit_separators() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT 1_000"), "1000");
    assert_eq!(text(&mut e, "SELECT 1_000_000"), "1000000");
    assert_eq!(text(&mut e, "SELECT (1_000.5)::text"), "1000.5");
    assert_eq!(text(&mut e, "SELECT 1_0e3"), "10000");
    // No separator unchanged; `..` range / subscript unaffected.
    assert_eq!(text(&mut e, "SELECT 1000"), "1000");
    assert_eq!(text(&mut e, "SELECT ((ARRAY[10,20,30])[1])::text"), "10");
}
