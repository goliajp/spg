//! v7.38 (read01) — PG `lseg # lseg` returns the crossing point of two line
//! segments (or NULL when they don't cross). Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn cell(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            spg_storage::Value::Null => "<NULL>".into(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn lseg_intersection_point() {
    let mut e = Engine::new();
    assert_eq!(
        cell(
            &mut e,
            "SELECT (lseg('[(0,0),(1,1)]') # lseg('[(0,1),(1,0)]'))::text"
        ),
        "(0.5,0.5)"
    );
    assert_eq!(
        cell(
            &mut e,
            "SELECT COALESCE((lseg('[(0,0),(1,0)]') # lseg('[(0,1),(1,1)]'))::text, '<NULL>')"
        ),
        "<NULL>"
    );
    // Integer XOR `#` is unaffected.
    assert_eq!(cell(&mut e, "SELECT (12 # 10)::text"), "6");
}
