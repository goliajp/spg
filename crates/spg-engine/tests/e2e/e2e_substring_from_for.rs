//! v7.37.7 C.1.8 — `substring(str FROM pos [FOR len])` PG syntactic form.
//! Desugars at parse time to the comma-list shape the evaluator
//! already handles.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn first_text(r: QueryResult) -> String {
    match r {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Text(s) => s.to_string(),
            other => panic!("expected Text, got {other:?}"),
        },
        _ => panic!("Rows"),
    }
}

#[test]
fn substring_from_for_matches_comma_form() {
    let mut e = Engine::new();
    let from_for = e
        .execute("SELECT substring('hello world' FROM 1 FOR 5)")
        .expect("substring FROM ... FOR ...");
    let comma = e
        .execute("SELECT substring('hello world', 1, 5)")
        .expect("substring(arg, arg, arg) form");
    assert_eq!(first_text(from_for), first_text(comma));
}

#[test]
fn substring_from_only_no_length() {
    let mut e = Engine::new();
    let from = e
        .execute("SELECT substring('abcdef' FROM 3)")
        .expect("substring str FROM start (no FOR)");
    let comma = e
        .execute("SELECT substring('abcdef', 3)")
        .expect("comma form 2-arg");
    assert_eq!(first_text(from), first_text(comma));
}

#[test]
fn substr_keyword_accepts_from_for_too() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT substr('xyz' FROM 2 FOR 1)")
        .expect("substr accepts the FROM/FOR form");
    assert_eq!(first_text(r), "y");
}

#[test]
fn substring_from_for_against_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, s TEXT)").unwrap();
    e.execute("INSERT INTO t VALUES (1, 'hello'), (2, 'world')")
        .unwrap();
    let r = e
        .execute("SELECT substring(s FROM 2 FOR 3) FROM t ORDER BY id")
        .expect("substring FROM/FOR on column");
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].values[0], Value::text("ell"));
            assert_eq!(rows[1].values[0], Value::text("orl"));
        }
        _ => panic!(),
    }
}
