//! v7.38 (read01 sweep) — PG 16 to_bin() / to_oct() render an integer in
//! binary / octal. Oracle behaviour from live PG 18.4.

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
fn to_bin_and_to_oct() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT to_bin(5)"), "101");
    assert_eq!(text(&mut e, "SELECT to_bin(255)"), "11111111");
    assert_eq!(text(&mut e, "SELECT to_oct(8)"), "10");
    assert_eq!(text(&mut e, "SELECT to_oct(64::bigint)"), "100");
    // to_hex is unchanged.
    assert_eq!(text(&mut e, "SELECT to_hex(255)"), "ff");
}
