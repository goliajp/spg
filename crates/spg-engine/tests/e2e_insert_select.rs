//! v7.13.0 — `INSERT INTO t [(cols)] SELECT …`.
//! mailrs round-5 G4 (3 hits).

use spg_engine::Engine;

#[test]
fn insert_select_copies_rows_from_one_table_to_another() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE src (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    eng.execute("CREATE TABLE dst (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    eng.execute("INSERT INTO src VALUES (1, 'alice'), (2, 'bob')")
        .unwrap();
    eng.execute("INSERT INTO dst SELECT id, name FROM src")
        .unwrap();
    let dst = eng.catalog().get("dst").expect("table present");
    assert_eq!(dst.rows().len(), 2);
}

#[test]
fn insert_select_with_explicit_column_list() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE src (a INT NOT NULL, b TEXT NOT NULL)")
        .unwrap();
    eng.execute("CREATE TABLE dst (name TEXT NOT NULL, num INT NOT NULL)")
        .unwrap();
    eng.execute("INSERT INTO src VALUES (10, 'x'), (20, 'y')")
        .unwrap();
    eng.execute("INSERT INTO dst (num, name) SELECT a, b FROM src")
        .unwrap();
    let dst = eng.catalog().get("dst").expect("table present");
    assert_eq!(dst.rows().len(), 2);
}

#[test]
fn insert_select_with_where_filters_rows() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE src (id INT NOT NULL)").unwrap();
    eng.execute("CREATE TABLE dst (id INT NOT NULL)").unwrap();
    eng.execute("INSERT INTO src VALUES (1), (2), (3), (4)").unwrap();
    eng.execute("INSERT INTO dst SELECT id FROM src WHERE id > 2")
        .unwrap();
    let dst = eng.catalog().get("dst").expect("table present");
    assert_eq!(dst.rows().len(), 2);
}

#[test]
fn insert_select_with_bool_and_int() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE src (n INT NOT NULL, flag BOOL NOT NULL)")
        .unwrap();
    eng.execute("CREATE TABLE dst (n INT NOT NULL, flag BOOL NOT NULL)")
        .unwrap();
    eng.execute("INSERT INTO src VALUES (1, TRUE), (2, FALSE)")
        .unwrap();
    eng.execute("INSERT INTO dst SELECT n, flag FROM src")
        .unwrap();
    let dst = eng.catalog().get("dst").expect("table present");
    assert_eq!(dst.rows().len(), 2);
}

#[test]
fn insert_select_empty_source_inserts_nothing() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE src (id INT NOT NULL)").unwrap();
    eng.execute("CREATE TABLE dst (id INT NOT NULL)").unwrap();
    eng.execute("INSERT INTO dst SELECT id FROM src").unwrap();
    let dst = eng.catalog().get("dst").expect("table present");
    assert_eq!(dst.rows().len(), 0);
}
