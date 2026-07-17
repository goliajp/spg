//! v7.39 (read01 round 129, PG-feature — ON CONFLICT RETURNING OLD) — PG18's
//! `INSERT … ON CONFLICT DO UPDATE … RETURNING OLD.*` returns the pre-update
//! conflicting row as OLD; locked byte-identical against PG 18.4.
//!
//! Closes the round-126 defer ("ON CONFLICT DO UPDATE's OLD reads NULL").
//! On the DO UPDATE path OLD = the conflicting row before the update; on the
//! plain-insert path (no conflict) OLD = NULL (no prior row). NEW / bare columns
//! keep the post-statement row (round 126).

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
    e.execute("CREATE TABLE oc(id int primary key, v int)")
        .unwrap();
    e.execute("INSERT INTO oc VALUES(1,10)").unwrap();
}

#[test]
fn on_conflict_update_path_old_is_pre_update() {
    let mut e = Engine::new();
    setup(&mut e);
    // id=1 conflicts → DO UPDATE v=oc.v+1: OLD.v=10 (pre-update), NEW.v=11, v=11.
    assert_eq!(
        row1(
            &mut e,
            "INSERT INTO oc VALUES(1,99) ON CONFLICT(id) DO UPDATE SET v=oc.v+1 \
             RETURNING OLD.v, NEW.v, v"
        ),
        vec!["10", "11", "11"]
    );
}

#[test]
fn on_conflict_insert_path_old_is_null() {
    let mut e = Engine::new();
    setup(&mut e);
    // id=2 does not conflict → plain insert: OLD.v=NULL, NEW.v=20, v=20.
    assert_eq!(
        row1(
            &mut e,
            "INSERT INTO oc VALUES(2,20) ON CONFLICT(id) DO UPDATE SET v=oc.v+1 \
             RETURNING OLD.v, NEW.v, v"
        ),
        vec!["NULL", "20", "20"]
    );
}

#[test]
fn on_conflict_update_old_star() {
    let mut e = Engine::new();
    setup(&mut e);
    // OLD.* = pre-update conflicting row (1,10); NEW.* = (1,11).
    assert_eq!(
        row1(
            &mut e,
            "INSERT INTO oc VALUES(1,99) ON CONFLICT(id) DO UPDATE SET v=oc.v+1 \
             RETURNING OLD.*, NEW.*"
        ),
        vec!["1", "10", "1", "11"]
    );
}

#[test]
fn plain_insert_old_is_null_regression() {
    let mut e = Engine::new();
    setup(&mut e);
    // No ON CONFLICT: OLD is still NULL, NEW is the inserted row.
    assert_eq!(
        row1(&mut e, "INSERT INTO oc VALUES(3,30) RETURNING OLD.v, NEW.v"),
        vec!["NULL", "30"]
    );
}

#[test]
fn multi_row_mixed_insert_and_conflict() {
    let mut e = Engine::new();
    setup(&mut e);
    // One row conflicts (id=1), one is new (id=5). RETURNING streams inserted
    // rows first then updated rows; check the count and both OLD images.
    match e
        .execute(
            "INSERT INTO oc VALUES(5,50),(1,99) ON CONFLICT(id) DO UPDATE SET v=oc.v+1 \
             RETURNING id, OLD.v, NEW.v",
        )
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            let got: Vec<Vec<String>> = rows
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
                .collect();
            // Inserted row (id=5): OLD=NULL, NEW=50. Updated row (id=1): OLD=10, NEW=11.
            assert!(
                got.contains(&vec!["5".into(), "NULL".into(), "50".into()]),
                "{got:?}"
            );
            assert!(
                got.contains(&vec!["1".into(), "10".into(), "11".into()]),
                "{got:?}"
            );
        }
        other => panic!("{other:?}"),
    }
}
