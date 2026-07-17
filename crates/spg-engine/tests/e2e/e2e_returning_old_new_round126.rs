//! v7.39 (read01 round 126, Track A — RETURNING OLD/NEW) — PG18's
//! `RETURNING OLD.col / NEW.col`, locked byte-identical against PG 18.4.
//!
//! Read-driven follow-up to the round-123 nodeModifyTable defer. `OLD.col` /
//! `NEW.col` in RETURNING now resolve against the pre/post rows: for UPDATE
//! OLD=pre / NEW=post, for DELETE NEW is NULL (the row is gone), for INSERT OLD
//! is NULL (no prior row). Bare columns and `table.col` keep the default
//! (NEW for INSERT/UPDATE, OLD for DELETE). Output column names/types come from
//! the referenced column (OLD.v → "v").
//!
//! Not covered (documented): `OLD.*` / `NEW.*` (SPG has no qualified wildcard),
//! and ON CONFLICT DO UPDATE's OLD (the pre-update conflicting row) reads NULL.

use spg_engine::{Engine, QueryResult};

fn row1(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(|v| match v {
                spg_storage::Value::Null => "NULL".to_string(),
                v => spg_engine::eval::value_to_text(v),
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE r(id int, v int)").unwrap();
    e.execute("INSERT INTO r VALUES(1,10)").unwrap();
}

#[test]
fn update_returning_old_new() {
    let mut e = Engine::new();
    setup(&mut e);
    // OLD.v=10, NEW.v=15, bare v=NEW=15, NEW.v-OLD.v=5.
    assert_eq!(
        row1(
            &mut e,
            "UPDATE r SET v=v+5 WHERE id=1 RETURNING OLD.v, NEW.v, v, NEW.v-OLD.v AS d"
        ),
        vec!["10", "15", "15", "5"]
    );
}

#[test]
fn delete_returning_new_is_null() {
    let mut e = Engine::new();
    setup(&mut e);
    // OLD.id=1, NEW.id=NULL (row gone), OLD.v=10.
    assert_eq!(
        row1(
            &mut e,
            "DELETE FROM r WHERE id=1 RETURNING OLD.id, NEW.id, OLD.v"
        ),
        vec!["1", "NULL", "10"]
    );
}

#[test]
fn insert_returning_old_is_null() {
    let mut e = Engine::new();
    setup(&mut e);
    // OLD.v=NULL (no prior row), NEW.v=20.
    assert_eq!(
        row1(
            &mut e,
            "INSERT INTO r VALUES(2,20) RETURNING OLD.v, NEW.v, id"
        ),
        vec!["NULL", "20", "2"]
    );
}

#[test]
fn plain_returning_unchanged() {
    let mut e = Engine::new();
    setup(&mut e);
    // Regression: RETURNING without OLD/NEW takes the plain fast path.
    assert_eq!(
        row1(&mut e, "UPDATE r SET v=99 WHERE id=1 RETURNING id, v"),
        vec!["1", "99"]
    );
}

#[test]
fn output_column_name_is_the_referenced_column() {
    let mut e = Engine::new();
    setup(&mut e);
    match e
        .execute("UPDATE r SET v=7 WHERE id=1 RETURNING OLD.v, NEW.v")
        .unwrap()
    {
        QueryResult::Rows { columns, .. } => {
            // PG names both columns "v".
            assert_eq!(columns[0].name, "v");
            assert_eq!(columns[1].name, "v");
        }
        other => panic!("{other:?}"),
    }
}
