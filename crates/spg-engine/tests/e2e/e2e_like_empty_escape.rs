//! v7.38 (read01 P6.18) — `LIKE ... ESCAPE ''` means "no escape character"
//! (every `%`/`_` is a wildcard, nothing is escaped), matching PG. A
//! multi-character escape is still an error. Oracle values from live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn b(e: &mut Engine, sql: &str) -> bool {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => {
            matches!(rows[0].values[0], spg_storage::Value::Bool(true))
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn empty_escape_disables_escaping() {
    let mut e = Engine::new();
    assert!(b(&mut e, "SELECT 'abc' LIKE 'abc' ESCAPE ''"));
    // `%` and `_` stay wildcards under an empty escape.
    assert!(b(&mut e, "SELECT 'axb' LIKE 'a%b' ESCAPE ''"));
    assert!(b(&mut e, "SELECT 'a%b' LIKE 'a%b' ESCAPE ''"));
    // A literal backslash is just a backslash (no escaping active).
    assert!(b(&mut e, r"SELECT 'a\b' LIKE 'a\b' ESCAPE ''"));
}

#[test]
fn single_char_escape_still_works() {
    let mut e = Engine::new();
    // `!` escapes the `%` so it must match a literal percent.
    assert!(b(&mut e, "SELECT 'a%b' LIKE 'a!%b' ESCAPE '!'"));
    assert!(!b(&mut e, "SELECT 'axb' LIKE 'a!%b' ESCAPE '!'"));
}

#[test]
fn multi_char_escape_is_rejected() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT 'abc' LIKE 'abc' ESCAPE 'xy'").is_err());
}
