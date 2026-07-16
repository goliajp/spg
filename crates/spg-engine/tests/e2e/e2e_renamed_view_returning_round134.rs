//! v7.39 (read01 round 134 — RETURNING through a column-renamed view) — closes
//! the round-133 residual: a write through a renamed auto-updatable view can
//! RETURN the view's columns. The refs map to the base columns for evaluation,
//! but the output name stays the view column (PG labels `RETURNING a` "a", not
//! the base "id"). Locked byte-identical against PG 18.4.

use spg_engine::{Engine, QueryResult};

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE rt(id int, x int)").unwrap();
    e.execute("CREATE VIEW rv(a, b) AS SELECT id, x FROM rt").unwrap();
}

fn run(e: &mut Engine, sql: &str) -> (Vec<String>, Vec<Vec<String>>) {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { columns, rows } => (
            columns.iter().map(|c| c.name.clone()).collect(),
            rows.iter()
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
        ),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn insert_returning_view_columns() {
    let mut e = Engine::new();
    setup(&mut e);
    // RETURNING a, b, b+1 AS bp — a→id, b→x for eval; names stay a/b/bp.
    let (cols, rows) = run(&mut e, "INSERT INTO rv(a,b) VALUES(1,10) RETURNING a, b, b+1 AS bp");
    assert_eq!(cols, vec!["a", "b", "bp"]);
    assert_eq!(rows, vec![vec!["1", "10", "11"]]);
}

#[test]
fn insert_returning_star() {
    let mut e = Engine::new();
    setup(&mut e);
    // RETURNING * → the view's columns (a, b) with base values.
    let (cols, rows) = run(&mut e, "INSERT INTO rv(a,b) VALUES(2,20) RETURNING *");
    assert_eq!(cols, vec!["a", "b"]);
    assert_eq!(rows, vec![vec!["2", "20"]]);
}

#[test]
fn update_returning_view_columns() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("INSERT INTO rv(a,b) VALUES(1,10)").unwrap();
    let (cols, rows) = run(&mut e, "UPDATE rv SET b=99 WHERE a=1 RETURNING a, b");
    assert_eq!(cols, vec!["a", "b"]);
    assert_eq!(rows, vec![vec!["1", "99"]]);
}

#[test]
fn delete_returning_view_columns() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("INSERT INTO rv(a,b) VALUES(1,10)").unwrap();
    let (cols, rows) = run(&mut e, "DELETE FROM rv WHERE a=1 RETURNING a, b");
    assert_eq!(cols, vec!["a", "b"]);
    assert_eq!(rows, vec![vec!["1", "10"]]);
}
