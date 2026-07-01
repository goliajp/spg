//! v7.37.17 (17.6 sibling) — SHOW ALL returns curated parameter
//! inventory as (name, setting, description) triples.

use spg_engine::{Engine, QueryResult};

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn show_all_returns_curated_parameter_list() {
    let mut e = Engine::new();
    let r = e.execute("SHOW ALL").unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("expected Rows");
    };
    assert_eq!(columns.len(), 3);
    assert_eq!(columns[0].name, "name");
    assert_eq!(columns[1].name, "setting");
    assert_eq!(columns[2].name, "description");
    assert!(rows.len() >= 10, "expected >=10 rows, got {}", rows.len());
    // spot check a few known params
    let names: alloc::vec::Vec<&str> = rows
        .iter()
        .map(|r| match &r.values[0] {
            spg_storage::Value::Text(s) => s.as_ref(),
            _ => "",
        })
        .collect();
    assert!(names.contains(&"server_version"));
    assert!(names.contains(&"client_encoding"));
    assert!(names.contains(&"search_path"));
    assert!(names.contains(&"transaction_isolation"));
}

#[test]
fn show_single_param_still_works() {
    let mut e = Engine::new();
    let r = e.execute("SHOW server_version").unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("expected Rows");
    };
    assert_eq!(columns.len(), 1);
    assert_eq!(rows.len(), 1);
    ddl(&mut e, "SET TimeZone = 'America/New_York'");
    let r = e.execute("SHOW TimeZone").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "America/New_York"),
        other => panic!("unexpected value {other:?}"),
    }
}

extern crate alloc;
