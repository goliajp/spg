//! v7.39 (read01 round 130, PG-feature — MERGE RETURNING) — PG17+'s
//! `MERGE … RETURNING` with `merge_action()`, `OLD.*`/`NEW.*`, and the
//! target/source aliases; locked byte-identical against PG 18.4.
//!
//! Closes the last of the round-126 RETURNING OLD/NEW backlog. `merge_action()`
//! yields 'INSERT'/'UPDATE'/'DELETE'; OLD is the pre-image (NULL for INSERT),
//! NEW the post-image (NULL for DELETE), `t.col`/`s.col` the target/source
//! aliases, and a bare `*` expands to source columns then target columns (the
//! MERGE range-table order). Returned rows follow source-row order.

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

#[test]
fn action_target_old_new() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE mt(id int primary key, v int)").unwrap();
    e.execute("INSERT INTO mt VALUES(1,10),(2,20)").unwrap();
    e.execute("CREATE TABLE ms(id int, v int)").unwrap();
    e.execute("INSERT INTO ms VALUES(1,100),(3,300)").unwrap();
    // s=(1,100) → UPDATE (OLD.v=10,NEW.v=100); s=(3,300) → INSERT (OLD.v=NULL,NEW.v=300).
    assert_eq!(
        rows(
            &mut e,
            "MERGE INTO mt t USING ms s ON t.id=s.id \
             WHEN MATCHED THEN UPDATE SET v=s.v \
             WHEN NOT MATCHED THEN INSERT VALUES(s.id,s.v) \
             RETURNING merge_action(), t.id, OLD.v, NEW.v"
        ),
        vec![
            vec!["UPDATE", "1", "10", "100"],
            vec!["INSERT", "3", "NULL", "300"],
        ]
    );
}

#[test]
fn old_star_new_star() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE mt(id int primary key, v int)").unwrap();
    e.execute("INSERT INTO mt VALUES(1,10)").unwrap();
    e.execute("CREATE TABLE ms(id int, v int)").unwrap();
    e.execute("INSERT INTO ms VALUES(1,100)").unwrap();
    // Matched UPDATE: OLD.* = (1,10), NEW.* = (1,100).
    assert_eq!(
        rows(
            &mut e,
            "MERGE INTO mt t USING ms s ON t.id=s.id \
             WHEN MATCHED THEN UPDATE SET v=s.v \
             RETURNING OLD.*, NEW.*"
        ),
        vec![vec!["1", "10", "1", "100"]]
    );
}

#[test]
fn delete_action_and_bare_star_ordering() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE mt(id int primary key, tv int)").unwrap();
    e.execute("INSERT INTO mt VALUES(9,90)").unwrap();
    e.execute("CREATE TABLE ms(sid int, sv int)").unwrap();
    e.execute("INSERT INTO ms VALUES(9,7)").unwrap();
    // DELETE; bare `*` = source cols (sid,sv) then target cols (id,tv).
    assert_eq!(
        rows(
            &mut e,
            "MERGE INTO mt t USING ms s ON t.id=s.sid \
             WHEN MATCHED THEN DELETE \
             RETURNING merge_action(), *"
        ),
        vec![vec!["DELETE", "9", "7", "9", "90"]]
    );
}

#[test]
fn returning_column_names() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE mt(id int primary key, v int)").unwrap();
    e.execute("INSERT INTO mt VALUES(1,10)").unwrap();
    e.execute("CREATE TABLE ms(id int, v int)").unwrap();
    e.execute("INSERT INTO ms VALUES(1,100)").unwrap();
    // merge_action() → "merge_action", t.id → "id", OLD.v/NEW.v → "v".
    assert_eq!(
        headers(
            &mut e,
            "MERGE INTO mt t USING ms s ON t.id=s.id \
             WHEN MATCHED THEN UPDATE SET v=s.v \
             RETURNING merge_action(), t.id, OLD.v, NEW.v"
        ),
        vec!["merge_action", "id", "v", "v"]
    );
}

#[test]
fn merge_without_returning_still_commandok() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE mt(id int primary key, v int)").unwrap();
    e.execute("INSERT INTO mt VALUES(1,10)").unwrap();
    e.execute("CREATE TABLE ms(id int, v int)").unwrap();
    e.execute("INSERT INTO ms VALUES(1,100)").unwrap();
    // Regression: no RETURNING → CommandOk, not Rows.
    match e
        .execute(
            "MERGE INTO mt t USING ms s ON t.id=s.id WHEN MATCHED THEN UPDATE SET v=s.v",
        )
        .unwrap()
    {
        QueryResult::CommandOk { affected, .. } => assert_eq!(affected, 1),
        other => panic!("{other:?}"),
    }
}
