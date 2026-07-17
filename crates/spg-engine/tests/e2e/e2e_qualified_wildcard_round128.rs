//! v7.39 (read01 round 128, Track A — qualified wildcard `q.*`) — PG18's
//! `qualifier.*` in the select list and in RETURNING, locked byte-identical
//! against PG 18.4.
//!
//! `t.*` expands to every column of table/alias `t`; in a join `a.*` expands
//! only the columns of peer `a`, labelled by their bare column name (PG drops
//! the `alias.` prefix). In RETURNING, `OLD.*` / `NEW.*` expand the pre-/post-
//! image, `t.*` expands the table columns. An unknown qualifier is an error.
//!
//! Closes the round-126 defer ("SPG has no qualified wildcard").
//!
//! Not covered (documented residual): multi-level `schema.table.*` (the parser
//! intercept is single-level `<ident>.*`).

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
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

fn headers(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { columns, .. } => columns.iter().map(|c| c.name.clone()).collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE wa(id int, v text)").unwrap();
    e.execute("CREATE TABLE wb(id int, w text)").unwrap();
    e.execute("INSERT INTO wa VALUES(1,'x'),(2,'y')").unwrap();
    e.execute("INSERT INTO wb VALUES(1,'p'),(2,'q')").unwrap();
}

#[test]
fn single_table_star() {
    let mut e = Engine::new();
    setup(&mut e);
    // SELECT wa.* FROM wa → all columns of wa.
    assert_eq!(
        rows(&mut e, "SELECT wa.* FROM wa ORDER BY id"),
        vec![vec!["1", "x"], vec!["2", "y"]]
    );
    assert_eq!(
        headers(&mut e, "SELECT wa.* FROM wa ORDER BY id"),
        vec!["id", "v"]
    );
}

#[test]
fn join_qualified_star_then_col() {
    let mut e = Engine::new();
    setup(&mut e);
    // SELECT wa.*, wb.w — wa's columns then wb.w; headers are bare names.
    let sql = "SELECT wa.*, wb.w FROM wa JOIN wb ON wa.id=wb.id ORDER BY wa.id";
    assert_eq!(
        rows(&mut e, sql),
        vec![vec!["1", "x", "p"], vec!["2", "y", "q"]]
    );
    assert_eq!(headers(&mut e, sql), vec!["id", "v", "w"]);
}

#[test]
fn join_both_stars() {
    let mut e = Engine::new();
    setup(&mut e);
    let sql = "SELECT wa.*, wb.* FROM wa JOIN wb ON wa.id=wb.id ORDER BY wa.id";
    assert_eq!(
        rows(&mut e, sql),
        vec![vec!["1", "x", "1", "p"], vec!["2", "y", "2", "q"]]
    );
}

#[test]
fn unknown_qualifier_errors() {
    let mut e = Engine::new();
    setup(&mut e);
    assert!(e.execute("SELECT zz.* FROM wa").is_err());
}

#[test]
fn returning_old_new_star() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE wr(id int, v text)").unwrap();
    e.execute("INSERT INTO wr VALUES(1,'a'),(2,'b')").unwrap();
    // OLD.* = pre-image (1,a), then new.v = z.
    assert_eq!(
        rows(
            &mut e,
            "UPDATE wr SET v='z' WHERE id=1 RETURNING OLD.*, NEW.v"
        ),
        vec![vec!["1", "a", "z"]]
    );
    // NEW.* = post-image (2,k).
    assert_eq!(
        rows(&mut e, "UPDATE wr SET v='k' WHERE id=2 RETURNING NEW.*"),
        vec![vec!["2", "k"]]
    );
}

#[test]
fn returning_old_star_headers() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE wr(id int, v text)").unwrap();
    e.execute("INSERT INTO wr VALUES(1,'a')").unwrap();
    // PG names OLD.* columns by their bare table-column names.
    assert_eq!(
        headers(
            &mut e,
            "UPDATE wr SET v='z' WHERE id=1 RETURNING OLD.*, NEW.v"
        ),
        vec!["id", "v", "v"]
    );
}
