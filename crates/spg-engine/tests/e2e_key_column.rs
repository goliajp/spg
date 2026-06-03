//! v7.9.16 — F4: column named `key` works (post-F1 + F4 sweep).

use spg_engine::{Engine, QueryResult};

#[test]
fn bare_key_as_column_name_in_create_table() {
    let mut eng = Engine::new();
    let r = eng.execute("CREATE TABLE t1 (key TEXT)");
    assert!(matches!(r, Ok(QueryResult::CommandOk { .. })), "{r:?}");
}

#[test]
fn bare_key_with_primary_key_constraint() {
    // mailrs greylist_triplets pattern.
    let mut eng = Engine::new();
    let r = eng.execute("CREATE TABLE greylist (key TEXT PRIMARY KEY)");
    assert!(matches!(r, Ok(QueryResult::CommandOk { .. })), "{r:?}");
}

#[test]
fn quoted_key_as_column_name() {
    let mut eng = Engine::new();
    let r = eng.execute(r#"CREATE TABLE t3 ("key" TEXT)"#);
    assert!(matches!(r, Ok(QueryResult::CommandOk { .. })), "{r:?}");
}

#[test]
fn quoted_key_with_primary_key() {
    let mut eng = Engine::new();
    let r = eng.execute(r#"CREATE TABLE t4 ("key" TEXT PRIMARY KEY)"#);
    assert!(matches!(r, Ok(QueryResult::CommandOk { .. })), "{r:?}");
}

#[test]
fn key_column_insert_and_select() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    eng.execute("INSERT INTO config VALUES ('host', 'localhost')")
        .unwrap();
    let r = eng.execute("SELECT value FROM config WHERE key = 'host'").unwrap();
    let QueryResult::Rows { rows, .. } = r else { panic!() };
    assert_eq!(rows.len(), 1);
}
