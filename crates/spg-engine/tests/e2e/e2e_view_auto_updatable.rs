//! v7.37.19 (19.13) — simple-query view auto-updatable
//! INSERT / UPDATE / DELETE redirect to base table.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
fn view_with_where_is_auto_updatable() {
    // v7.38 (read01 P6.46) — a simple single-table view WITH a WHERE is
    // auto-updatable in PG: the view's WHERE is AND-ed onto UPDATE/DELETE so
    // only rows visible through the view are touched, and INSERT goes straight
    // to the base (no WITH CHECK OPTION, so it need not satisfy the WHERE).
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE base (id INT, name TEXT)");
    e.execute("INSERT INTO base VALUES (5, 'lo'), (20, 'hi')")
        .unwrap();
    ddl(
        &mut e,
        "CREATE VIEW v AS SELECT id, name FROM base WHERE id > 10",
    );

    // UPDATE only touches rows visible through the view (id > 10).
    e.execute("UPDATE v SET name = 'HI' WHERE id = 20").unwrap();
    e.execute("UPDATE v SET name = 'nope' WHERE id = 5")
        .unwrap(); // filtered out
    let names = |e: &mut Engine, sql: &str| match e.execute(sql).unwrap() {
        spg_engine::QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!(),
    };
    assert_eq!(names(&mut e, "SELECT name FROM base WHERE id = 20"), "HI");
    assert_eq!(names(&mut e, "SELECT name FROM base WHERE id = 5"), "lo"); // untouched

    // INSERT succeeds (no CHECK OPTION), landing in the base table.
    e.execute("INSERT INTO v (id, name) VALUES (30, 'new')")
        .unwrap();
    assert_eq!(names(&mut e, "SELECT name FROM base WHERE id = 30"), "new");

    // DELETE only removes view-visible rows.
    e.execute("DELETE FROM v WHERE id = 5").unwrap(); // filtered out, no-op
    assert_eq!(names(&mut e, "SELECT name FROM base WHERE id = 5"), "lo");
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
