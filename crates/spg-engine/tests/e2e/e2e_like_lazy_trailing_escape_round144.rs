//! v7.39 (read01 round 144, like_match.c) — PG raises the trailing-escape
//! LIKE error (22025) LAZILY, inside the matcher: only a branch that reaches
//! the trailing `\` with text left errors. A branch where the text is already
//! exhausted returns false without ever "seeing" it — so `'x' LIKE 'x\'` is
//! FALSE while `'xy' LIKE 'x\'` errors. SPG previously pre-checked the
//! pattern and errored eagerly on all of them; worse, the compiled table-scan
//! VM path had no check at all and silently matched a trailing `\` as a
//! literal. Locked byte-identical against PG 18.4 (probed live, 13 cases).

use spg_engine::{Engine, QueryResult};

fn b(e: &mut Engine, sql: &str) -> Option<bool> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            spg_storage::Value::Bool(x) => Some(x),
            spg_storage::Value::Null => None,
            ref other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn errs(e: &mut Engine, sql: &str) {
    let m = match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(_) => panic!("expected error for {sql}"),
    };
    assert!(m.contains("LIKE pattern must not end with escape character"), "{m}");
}

#[test]
fn text_exhausted_before_trailing_escape_is_false() {
    let mut e = Engine::new();
    assert_eq!(b(&mut e, r"SELECT 'x' LIKE 'x\'"), Some(false));
    assert_eq!(b(&mut e, r"SELECT '' LIKE '\'"), Some(false));
    assert_eq!(b(&mut e, r"SELECT 'x' LIKE 'x%\'"), Some(false));
    assert_eq!(b(&mut e, r"SELECT 'ab' LIKE 'ab\'"), Some(false));
    // NOT LIKE / ILIKE see the same lazy false, negated or folded.
    assert_eq!(b(&mut e, r"SELECT 'x' NOT LIKE 'x\'"), Some(true));
    assert_eq!(b(&mut e, r"SELECT 'X' ILIKE 'x\'"), Some(false));
}

#[test]
fn matcher_reaching_trailing_escape_with_text_left_errors() {
    let mut e = Engine::new();
    errs(&mut e, r"SELECT 'xy' LIKE 'x\'");
    errs(&mut e, r"SELECT 'x' LIKE '\'");
    errs(&mut e, r"SELECT 'x' LIKE '%\'");
    errs(&mut e, r"SELECT 'abc' LIKE 'ab\'");
}

#[test]
fn escaped_backslash_still_matches_literally() {
    let mut e = Engine::new();
    assert_eq!(b(&mut e, r"SELECT 'x' LIKE 'x\\'"), Some(false));
    assert_eq!(b(&mut e, r"SELECT 'x\' LIKE 'x\\'"), Some(true));
}

#[test]
fn compiled_table_scan_path_matches_pg() {
    // The Step::Like VM path (literal pattern over a table scan) previously
    // treated a trailing `\` as a literal char — silent-wrong vs PG.
    let mut e = Engine::new();
    e.execute("CREATE TABLE lt(s text)").unwrap();
    e.execute("INSERT INTO lt VALUES ('x'),('xy')").unwrap();
    // The scan reaches row 'xy' whose match hits the trailing escape → error.
    errs(&mut e, r"SELECT s FROM lt WHERE s LIKE 'x\'");
    // With only text-exhausted rows the scan completes with zero matches.
    e.execute("DELETE FROM lt WHERE s = 'xy'").unwrap();
    match e.execute(r"SELECT s FROM lt WHERE s LIKE 'x\'").unwrap() {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 0),
        other => panic!("{other:?}"),
    }
}
