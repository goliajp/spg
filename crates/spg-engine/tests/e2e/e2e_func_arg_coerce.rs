//! v7.38 (read01, T16) — an unknown-type string literal in any function
//! argument position coerces to the parameter's type (PG resolves
//! `round('3.567', 2)`, `power('2','3')`, `left('hello','3')`). Ambiguous names
//! (trunc / mod / div) still error. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("rows"),
    }
}

#[test]
fn function_arg_string_coercion() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT (round('3.567', 2))::text"), "3.57");
    // power / log resolve to double for unknown-string args (result 8, not numeric).
    assert_eq!(text(&mut e, "SELECT (power('2','3'))::text"), "8");
    assert_eq!(text(&mut e, "SELECT (log('100'))::text"), "2");
    assert_eq!(text(&mut e, "SELECT left('hello', '3')"), "hel");
    assert_eq!(text(&mut e, "SELECT right('hello','2')"), "lo");
    assert_eq!(text(&mut e, "SELECT repeat('ab','3')"), "ababab");
    assert_eq!(text(&mut e, "SELECT (abs('-7'))::text"), "7");
    // Non-string args unaffected.
    assert_eq!(text(&mut e, "SELECT (round(3.567, 2))::text"), "3.57");
    assert_eq!(text(&mut e, "SELECT left('hello', 3)"), "hel");
}
