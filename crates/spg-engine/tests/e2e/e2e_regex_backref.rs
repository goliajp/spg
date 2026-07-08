//! v7.38 (read01, T7-br) — in-pattern regex backreferences `\1`..`\9`: match the
//! text captured by an earlier group, forcing backtracking through a quantified
//! group when needed, honoring case-insensitivity, and rejecting a
//! forward/unopened reference. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn b(e: &mut Engine, sql: &str) -> bool {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => matches!(rows[0].values[0], spg_storage::Value::Bool(true)),
        _ => panic!("rows"),
    }
}

#[test]
fn regex_backreferences() {
    let mut e = Engine::new();
    assert!(b(&mut e, r"SELECT 'abab' ~ '(ab)\1'"));
    assert!(!b(&mut e, r"SELECT 'abcd' ~ '(ab)\1'"));
    assert!(b(&mut e, r"SELECT 'aa' ~ '(.)\1'"));
    assert!(!b(&mut e, r"SELECT 'ab' ~ '(.)\1'"));
    // Backref forces backtracking through the group's greedy quantifier.
    assert!(b(&mut e, r"SELECT 'aaaa' ~ '^(a*)\1$'"));
    // Any group index.
    assert!(b(&mut e, r"SELECT 'abcb' ~ '(a(b)c)\2'"));
    // Case-insensitive (~*).
    assert!(b(&mut e, r"SELECT 'aA' ~* '(a)\1'"));
    // A forward/unopened backreference is a compile error.
    assert!(e.execute(r"SELECT 'x' ~ '\1(a)'").is_err());
    // Non-backref patterns are unaffected.
    assert!(b(&mut e, "SELECT 'abc' ~ 'b'"));
}
