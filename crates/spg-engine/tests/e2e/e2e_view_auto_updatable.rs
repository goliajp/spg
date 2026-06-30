//! v7.37.19 (19.13) — simple-query view auto-updatable
//! INSERT / UPDATE / DELETE redirect to base table.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("Rows");
    };
    rows.into_iter().map(|r| r.values).collect()
}

#[test]
fn insert_into_simple_view_lands_in_base_table() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE base (id INT, name TEXT)");
    ddl(&mut e, "CREATE VIEW v AS SELECT id, name FROM base");
    ddl(&mut e, "INSERT INTO v (id, name) VALUES (1, 'alice')");
    let rs = rows(&mut e, "SELECT id, name FROM base ORDER BY id");
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0][0], Value::Int(1));
    assert!(matches!(&rs[0][1], Value::Text(s) if s == "alice"));
}

#[test]
fn update_through_simple_view_updates_base() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE base (id INT, name TEXT)");
    ddl(&mut e, "INSERT INTO base (id, name) VALUES (1, 'old')");
    ddl(&mut e, "CREATE VIEW v AS SELECT id, name FROM base");
    ddl(&mut e, "UPDATE v SET name = 'new' WHERE id = 1");
    let rs = rows(&mut e, "SELECT name FROM base WHERE id = 1");
    assert!(matches!(&rs[0][0], Value::Text(s) if s == "new"));
}

#[test]
fn delete_through_simple_view_removes_from_base() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE base (id INT, name TEXT)");
    ddl(&mut e, "INSERT INTO base (id, name) VALUES (1, 'doomed')");
    ddl(&mut e, "INSERT INTO base (id, name) VALUES (2, 'survives')");
    ddl(&mut e, "CREATE VIEW v AS SELECT id, name FROM base");
    ddl(&mut e, "DELETE FROM v WHERE id = 1");
    let rs = rows(&mut e, "SELECT id FROM base ORDER BY id");
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0][0], Value::Int(2));
}

#[test]
fn view_with_where_is_not_auto_updatable() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE base (id INT, name TEXT)");
    ddl(&mut e, "CREATE VIEW v AS SELECT id, name FROM base WHERE id > 10");
    // INSERT should fail because the view isn't simple-query auto-updatable.
    let err = e.execute("INSERT INTO v (id, name) VALUES (1, 'x')");
    assert!(err.is_err(), "expected error inserting into non-auto-updatable view");
}

#[test]
fn view_with_join_is_not_auto_updatable() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE a (id INT, name TEXT)");
    ddl(&mut e, "CREATE TABLE b (id INT, kind TEXT)");
    ddl(
        &mut e,
        "CREATE VIEW v AS SELECT a.id, a.name FROM a INNER JOIN b ON a.id = b.id",
    );
    let err = e.execute("INSERT INTO v (id, name) VALUES (1, 'x')");
    assert!(err.is_err(), "expected error inserting into JOIN view");
}

#[test]
fn view_with_aggregate_is_not_auto_updatable() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE base (id INT, name TEXT)");
    ddl(&mut e, "CREATE VIEW v AS SELECT COUNT(*) FROM base");
    let err = e.execute("INSERT INTO v (id) VALUES (1)");
    assert!(err.is_err(), "expected error inserting into aggregate view");
}
