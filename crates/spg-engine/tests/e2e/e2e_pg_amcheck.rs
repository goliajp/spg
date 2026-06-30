//! v7.37.22 (22.4) — PG amcheck-extension equivalents.
//! `verify_heapam(text)` and `bt_index_check(text)` walk SPG's
//! catalog + storage and return NULL on a clean check or a TEXT
//! message describing the first issue.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter().next().expect("one row").values.into_iter().next().expect("one col")
}

#[test]
fn verify_heapam_returns_null_for_healthy_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)").unwrap();
    e.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();
    assert!(matches!(
        one(&mut e, "SELECT verify_heapam('t')"),
        Value::Null
    ));
}

#[test]
fn verify_heapam_returns_message_for_missing_table() {
    let mut e = Engine::new();
    let v = one(&mut e, "SELECT verify_heapam('does_not_exist')");
    if let Value::Text(s) = v {
        assert!(s.contains("does not exist"), "msg: {s:?}");
    } else {
        panic!("expected Text, got {v:?}");
    }
}

#[test]
fn bt_index_check_returns_null_when_no_indices() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    assert!(matches!(
        one(&mut e, "SELECT bt_index_check('t')"),
        Value::Null
    ));
}

#[test]
fn bt_index_check_returns_null_with_btree_index() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)").unwrap();
    e.execute("CREATE INDEX ix_t_name ON t(name)").unwrap();
    assert!(matches!(
        one(&mut e, "SELECT bt_index_check('t')"),
        Value::Null
    ));
}

#[test]
fn verify_heapam_returns_null_on_null_input() {
    // PG: NULL → NULL.
    let mut e = Engine::new();
    assert!(matches!(
        one(&mut e, "SELECT verify_heapam(NULL)"),
        Value::Null
    ));
    assert!(matches!(
        one(&mut e, "SELECT bt_index_check(NULL)"),
        Value::Null
    ));
}

#[test]
fn spg_aliased_variants_exist() {
    // SPG-prefixed aliases so spgctl-shaped tooling doesn't have
    // to know about the PG extension namespace.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    assert!(matches!(
        one(&mut e, "SELECT spg_verify_heapam('t')"),
        Value::Null
    ));
    assert!(matches!(
        one(&mut e, "SELECT spg_bt_index_check('t')"),
        Value::Null
    ));
}
