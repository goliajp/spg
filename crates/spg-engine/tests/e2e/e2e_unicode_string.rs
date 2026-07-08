//! v7.38 (read01, T18) — PG `U&'...'` Unicode string literals: `\XXXX` (4 hex),
//! `\+XXXXXX` (6 hex), `\\` → backslash, `''` → quote. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            spg_storage::Value::Int(n) => n.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("rows"),
    }
}

#[test]
fn unicode_string_literals() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, r"SELECT U&'\00E9'"), "é");
    assert_eq!(one(&mut e, r"SELECT U&'d\0061t\+000061'"), "data");
    assert_eq!(one(&mut e, r"SELECT U&'A\0042C'"), "ABC");
    assert_eq!(one(&mut e, r"SELECT length(U&'\00E9')"), "1");
    assert_eq!(one(&mut e, r"SELECT U&'hello'"), "hello");
    assert_eq!(one(&mut e, r"SELECT U&'a\\b'"), r"a\b");
    // A plain `u`/`U` identifier or string is unaffected.
    assert_eq!(one(&mut e, "SELECT 'u' || 'x'"), "ux");
}
