//! v7.39 (read01 round 205) — JSON_TABLE epic, Phase 0: parser + AST +
//! executor for the core shapes. All byte-identical vs live PG18.4
//! (2026-07-18): basic COLUMNS, FOR ORDINALITY, missing column → NULL,
//! DEFAULT ... ON EMPTY, ERROR ON ERROR, NESTED PATH outer-join
//! expansion, EXISTS PATH, root scalar, empty array, implicit LATERAL.
//! (FORMAT JSON / WITH WRAPPER = Phase 1.)

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
                        spg_storage::Value::BigInt(n) => n.to_string(),
                        spg_storage::Value::Bool(b) => {
                            if *b { "t" } else { "f" }.to_string()
                        }
                        other => format!("{other:?}"),
                    })
                    .collect()
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(er) => er.to_string(),
        Ok(r) => panic!("expected error, got {r:?}"),
    }
}

#[test]
fn basic_columns() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM json_table('[{\"a\":1,\"b\":\"x\"},{\"a\":2,\"b\":\"y\"}]', \
             '$[*]' COLUMNS (a INT PATH '$.a', b TEXT PATH '$.b')) jt"
        ),
        vec![vec!["1", "x"], vec!["2", "y"]]
    );
}

#[test]
fn for_ordinality() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM json_table('[10,20,30]', '$[*]' \
             COLUMNS (n FOR ORDINALITY, v INT PATH '$')) jt"
        ),
        vec![vec!["1", "10"], vec!["2", "20"], vec!["3", "30"]]
    );
}

#[test]
fn missing_column_is_null() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM json_table('[{\"a\":1}]', '$[*]' \
             COLUMNS (a INT PATH '$.a', b TEXT PATH '$.b')) jt"
        ),
        vec![vec!["1", "NULL"]]
    );
}

#[test]
fn default_on_empty() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM json_table('[{\"a\":1}]', '$[*]' \
             COLUMNS (b TEXT PATH '$.b' DEFAULT 'none' ON EMPTY)) jt"
        ),
        vec![vec!["none"]]
    );
}

#[test]
fn error_on_error() {
    let mut e = Engine::new();
    let m = err(
        &mut e,
        "SELECT * FROM json_table('[{\"a\":\"notint\"}]', '$[*]' \
         COLUMNS (a INT PATH '$.a' ERROR ON ERROR)) jt",
    );
    assert!(
        m.contains("invalid input syntax for type integer"),
        "unexpected: {m}"
    );
}

#[test]
fn error_default_is_null_by_default() {
    // Without ERROR ON ERROR, a coercion failure is NULL (PG default).
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM json_table('[{\"a\":\"notint\"}]', '$[*]' \
             COLUMNS (a INT PATH '$.a')) jt"
        ),
        vec![vec!["NULL"]]
    );
}

#[test]
fn nested_path_outer_join() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM json_table('[{\"id\":1,\"kids\":[{\"k\":\"a\"},{\"k\":\"b\"}]}]', \
             '$[*]' COLUMNS (id INT PATH '$.id', \
             NESTED PATH '$.kids[*]' COLUMNS (k TEXT PATH '$.k'))) jt"
        ),
        vec![vec!["1", "a"], vec!["1", "b"]]
    );
}

#[test]
fn nested_empty_keeps_parent() {
    // A parent with no nested match still emits one row (nested NULL).
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM json_table('[{\"id\":1,\"kids\":[]}]', \
             '$[*]' COLUMNS (id INT PATH '$.id', \
             NESTED PATH '$.kids[*]' COLUMNS (k TEXT PATH '$.k'))) jt"
        ),
        vec![vec!["1", "NULL"]]
    );
}

#[test]
fn exists_path() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM json_table('[{\"a\":1},{\"b\":2}]', '$[*]' \
             COLUMNS (has_a BOOL EXISTS PATH '$.a')) jt"
        ),
        vec![vec!["t"], vec!["f"]]
    );
}

#[test]
fn root_scalar_and_empty() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM json_table('{\"a\":1}', '$' COLUMNS (a INT PATH '$.a')) jt"
        ),
        vec![vec!["1"]]
    );
    assert!(rows(
        &mut e,
        "SELECT * FROM json_table('[]', '$[*]' COLUMNS (a INT PATH '$.a')) jt"
    )
    .is_empty());
}

#[test]
fn implicit_lateral() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT t.id, jt.k FROM (VALUES (1, '[{\"k\":\"x\"},{\"k\":\"y\"}]'::jsonb)) t(id, arr), \
             json_table(t.arr, '$[*]' COLUMNS (k TEXT PATH '$.k')) jt"
        ),
        vec![vec!["1", "x"], vec!["1", "y"]]
    );
}
