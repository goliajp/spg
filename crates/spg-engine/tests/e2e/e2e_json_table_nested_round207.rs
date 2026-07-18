//! v7.39 (read01 round 207) — JSON_TABLE Phase 2: multi-sibling and
//! deep NESTED PATH. PG sibling-NESTED semantics (byte-identical vs
//! live PG18.4, 2026-07-18): each sibling expands independently and
//! the rows CONCATENATE (a sibling's row fills only its cells, others
//! NULL); an empty sibling contributes ZERO rows; only when EVERY
//! sibling is empty does the parent emit one all-NULL nested row.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Null => "NULL".to_string(),
                        spg_storage::Value::Text(s) => s.to_string(),
                        spg_storage::Value::Int(n) => n.to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect()
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn two_siblings_concatenate() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM json_table('[{\"id\":1,\"a\":[{\"x\":10}],\"b\":[{\"y\":20},{\"y\":21}]}]', \
             '$[*]' COLUMNS (id INT PATH '$.id', \
             NESTED PATH '$.a[*]' COLUMNS (x INT PATH '$.x'), \
             NESTED PATH '$.b[*]' COLUMNS (y INT PATH '$.y'))) jt"
        ),
        vec![
            vec!["1", "10", "NULL"],
            vec!["1", "NULL", "20"],
            vec!["1", "NULL", "21"],
        ]
    );
}

#[test]
fn empty_sibling_contributes_nothing() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM json_table('[{\"id\":1,\"a\":[{\"x\":10}],\"b\":[]}]', \
             '$[*]' COLUMNS (id INT PATH '$.id', \
             NESTED PATH '$.a[*]' COLUMNS (x INT PATH '$.x'), \
             NESTED PATH '$.b[*]' COLUMNS (y INT PATH '$.y'))) jt"
        ),
        vec![vec!["1", "10", "NULL"]]
    );
}

#[test]
fn all_siblings_empty_one_null_row() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM json_table('[{\"id\":1,\"a\":[],\"b\":[]}]', \
             '$[*]' COLUMNS (id INT PATH '$.id', \
             NESTED PATH '$.a[*]' COLUMNS (x INT PATH '$.x'), \
             NESTED PATH '$.b[*]' COLUMNS (y INT PATH '$.y'))) jt"
        ),
        vec![vec!["1", "NULL", "NULL"]]
    );
}

#[test]
fn deep_nesting() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM json_table('[{\"id\":1,\"g\":[{\"gid\":9,\"h\":[{\"hv\":\"p\"},{\"hv\":\"q\"}]}]}]', \
             '$[*]' COLUMNS (id INT PATH '$.id', \
             NESTED PATH '$.g[*]' COLUMNS (gid INT PATH '$.gid', \
             NESTED PATH '$.h[*]' COLUMNS (hv TEXT PATH '$.hv')))) jt"
        ),
        vec![vec!["1", "9", "p"], vec!["1", "9", "q"]]
    );
}
