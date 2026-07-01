//! v7.37.17 (17.6 sibling) — TRUNCATE [TABLE] [ONLY] <name> [, ...]
//! [RESTART IDENTITY | CONTINUE IDENTITY] [CASCADE | RESTRICT].

use spg_engine::{Engine, QueryResult};

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn count(e: &mut Engine, table: &str) -> i64 {
    let r = e
        .execute(&alloc_string_fmt(table))
        .expect("SELECT COUNT");
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::Int(v) => *v as i64,
        spg_storage::Value::BigInt(v) => *v,
        other => panic!("unexpected count value {other:?}"),
    }
}

fn alloc_string_fmt(table: &str) -> String {
    format!("SELECT COUNT(*) FROM {table}")
}

#[test]
fn truncate_removes_all_rows() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (1), (2), (3)");
    assert_eq!(count(&mut e, "t"), 3);
    ddl(&mut e, "TRUNCATE t");
    assert_eq!(count(&mut e, "t"), 0);
}

#[test]
fn truncate_table_variant() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (1)");
    ddl(&mut e, "TRUNCATE TABLE t");
    assert_eq!(count(&mut e, "t"), 0);
}

#[test]
fn truncate_multiple_tables() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE a (id INT)");
    ddl(&mut e, "CREATE TABLE b (id INT)");
    ddl(&mut e, "INSERT INTO a VALUES (1), (2)");
    ddl(&mut e, "INSERT INTO b VALUES (10), (20), (30)");
    ddl(&mut e, "TRUNCATE a, b");
    assert_eq!(count(&mut e, "a"), 0);
    assert_eq!(count(&mut e, "b"), 0);
}

#[test]
fn truncate_with_restart_identity_cascade_parses() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (1)");
    ddl(&mut e, "TRUNCATE TABLE t RESTART IDENTITY CASCADE");
    assert_eq!(count(&mut e, "t"), 0);
}

#[test]
fn truncate_only_and_continue_identity_parses() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (1)");
    ddl(&mut e, "TRUNCATE TABLE ONLY t CONTINUE IDENTITY RESTRICT");
    assert_eq!(count(&mut e, "t"), 0);
}
