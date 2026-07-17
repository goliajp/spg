//! v7.39 (read01 round 133, PG-feature — renamed-column auto-updatable views) —
//! a simple view with a column-rename list `CREATE VIEW v(a,b) AS SELECT id, x
//! FROM t` is auto-updatable in PG: INSERT / UPDATE / DELETE through it map the
//! view's column names back to the base columns. SPG previously bailed
//! (`view_redirect` returned None for a renamed view) so writes errored.
//! Locked byte-identical against PG 18.4.
//!
//! Covers rename, reorder (SELECT x, id), subset (fewer view cols than base),
//! and the round-132 WITH CHECK OPTION combined with a rename.

use spg_engine::{Engine, QueryResult};

fn base_rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Null => "NULL".to_string(),
                        v => spg_engine::eval::value_to_text(v),
                    })
                    .collect()
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn insert_update_delete_through_renamed_view() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE rt(id int, x int)").unwrap();
    e.execute("CREATE VIEW rv(a, b) AS SELECT id, x FROM rt")
        .unwrap();
    // INSERT with explicit renamed column list.
    e.execute("INSERT INTO rv(a,b) VALUES(1,10),(2,20)")
        .unwrap();
    // INSERT positional through the view.
    e.execute("INSERT INTO rv VALUES(3,30)").unwrap();
    // UPDATE via renamed columns: b→x, a→id.
    e.execute("UPDATE rv SET b=99 WHERE a=1").unwrap();
    // DELETE via renamed column.
    e.execute("DELETE FROM rv WHERE a=2").unwrap();
    // Reads through the view show renamed columns.
    assert_eq!(
        base_rows(&mut e, "SELECT a,b FROM rv ORDER BY a"),
        vec![vec!["1", "99"], vec!["3", "30"]]
    );
    // Base table reflects the mapped writes.
    assert_eq!(
        base_rows(&mut e, "SELECT id,x FROM rt ORDER BY id"),
        vec![vec!["1", "99"], vec!["3", "30"]]
    );
}

#[test]
fn reordered_renamed_view() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE rt(id int, x int)").unwrap();
    // View projects x first, id second, renamed (a,b): a→x, b→id.
    e.execute("CREATE VIEW rv2(a, b) AS SELECT x, id FROM rt")
        .unwrap();
    e.execute("INSERT INTO rv2(a,b) VALUES(7,8)").unwrap();
    // a=7 lands in x, b=8 lands in id.
    assert_eq!(
        base_rows(&mut e, "SELECT id,x FROM rt"),
        vec![vec!["8", "7"]]
    );
}

#[test]
fn subset_renamed_view_insert() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE rt(id int, x int)").unwrap();
    e.execute("CREATE VIEW rs(only_id) AS SELECT id FROM rt")
        .unwrap();
    // Only the mapped column is set; the rest defaults to NULL.
    e.execute("INSERT INTO rs(only_id) VALUES(42)").unwrap();
    assert_eq!(
        base_rows(&mut e, "SELECT id,x FROM rt"),
        vec![vec!["42", "NULL"]]
    );
}

#[test]
fn renamed_view_with_check_option() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE rt(id int, x int)").unwrap();
    e.execute("CREATE VIEW rc(a, b) AS SELECT id, x FROM rt WHERE x > 0 WITH CHECK OPTION")
        .unwrap();
    // b maps to x; b=-5 fails the view's WHERE (x > 0).
    let msg = match e.execute("INSERT INTO rc(a,b) VALUES(1,-5)") {
        Err(x) => format!("{x}"),
        Ok(_) => panic!("expected check-option violation"),
    };
    assert!(
        msg.contains("new row violates check option for view \"rc\""),
        "{msg}"
    );
    // A satisfying row lands.
    e.execute("INSERT INTO rc(a,b) VALUES(2,5)").unwrap();
    assert_eq!(
        base_rows(&mut e, "SELECT id,x FROM rt"),
        vec![vec!["2", "5"]]
    );
}
