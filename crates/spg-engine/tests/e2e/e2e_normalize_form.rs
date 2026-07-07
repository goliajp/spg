//! v7.38 (read01 sweep) — normalize(text, FORM) with a bare form keyword
//! (NFC / NFD / NFKC / NFKD), the way PG spells it. Oracle from live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn int(e: &mut Engine, sql: &str) -> i32 {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Int(n) => *n,
            v => panic!("expected int, got {v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

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
fn normalize_accepts_bare_form_keyword() {
    let mut e = Engine::new();
    // 'café' with a precomposed é: NFD decomposes it (5 chars), NFC keeps it (4).
    assert_eq!(int(&mut e, "SELECT length(normalize('café', NFD))"), 5);
    assert_eq!(int(&mut e, "SELECT length(normalize('café', NFC))"), 4);
    assert_eq!(text(&mut e, "SELECT normalize('x', NFKC)"), "x");
    assert_eq!(text(&mut e, "SELECT normalize('x', NFKD)"), "x");
    // The string form and the one-arg form still work.
    assert_eq!(text(&mut e, "SELECT normalize('a', 'NFC')"), "a");
    assert_eq!(text(&mut e, "SELECT normalize('a')"), "a");
}
