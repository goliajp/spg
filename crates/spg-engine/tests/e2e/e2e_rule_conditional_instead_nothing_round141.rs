//! v7.39 (read01 round 141, CREATE RULE — Phase 3) — conditional
//! `ON <event> ... WHERE <cond> DO INSTEAD NOTHING`. Only the rows the
//! condition holds for are blocked; the rest run normally. The condition sees
//! NEW as the post-image (UPDATE: SET applied; INSERT: defaults applied), so
//! it is rewritten into a base-column predicate and pushed into the statement's
//! WHERE (`COALESCE(NOT(cond), TRUE)` — NULL means "rule does not apply").
//! Locked byte-identical against PG 18.4.

use spg_engine::{Engine, QueryResult};

fn pairs(e: &mut Engine, sql: &str) -> Vec<(i32, Option<i32>)> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                let a = match &r.values[0] {
                    spg_storage::Value::Int(a) => *a,
                    other => panic!("{other:?}"),
                };
                let b = match &r.values[1] {
                    spg_storage::Value::Int(b) => Some(*b),
                    spg_storage::Value::Null => None,
                    other => panic!("{other:?}"),
                };
                (a, b)
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn affected(e: &mut Engine, sql: &str) -> usize {
    match e.execute(sql).unwrap() {
        QueryResult::CommandOk { affected, .. } => affected,
        other => panic!("{other:?}"),
    }
}

#[test]
fn delete_conditional_over_old() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d(id int, v int)").unwrap();
    e.execute("INSERT INTO d VALUES (1,10),(2,20),(3,30)").unwrap();
    e.execute("CREATE RULE rd AS ON DELETE TO d WHERE OLD.v > 15 DO INSTEAD NOTHING")
        .unwrap();
    // Only id=1 (v=10, not > 15) is deleted; 2 and 3 are blocked.
    assert_eq!(affected(&mut e, "DELETE FROM d"), 1);
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM d ORDER BY id"),
        vec![(2, Some(20)), (3, Some(30))]
    );
}

#[test]
fn update_conditional_new_post_image_partial() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u(id int, v int)").unwrap();
    e.execute("INSERT INTO u VALUES (1,10),(2,20),(3,30)").unwrap();
    e.execute("CREATE RULE ru AS ON UPDATE TO u WHERE NEW.v < 0 DO INSTEAD NOTHING")
        .unwrap();
    // v-25: 10->-15 (blocked), 20->-5 (blocked), 30->5 (ok). UPDATE 1.
    assert_eq!(affected(&mut e, "UPDATE u SET v = v - 25"), 1);
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM u ORDER BY id"),
        vec![(1, Some(10)), (2, Some(20)), (3, Some(5))]
    );
}

#[test]
fn update_conditional_new_all_pass() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u(id int, v int)").unwrap();
    e.execute("INSERT INTO u VALUES (1,-5),(2,7),(3,-1)").unwrap();
    e.execute("CREATE RULE ru AS ON UPDATE TO u WHERE NEW.v < 0 DO INSTEAD NOTHING")
        .unwrap();
    // v+100: all post-images positive → nothing blocked → all 3 updated.
    assert_eq!(affected(&mut e, "UPDATE u SET v = v + 100"), 3);
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM u ORDER BY id"),
        vec![(1, Some(95)), (2, Some(107)), (3, Some(99))]
    );
}

#[test]
fn update_conditional_over_old() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE w(id int, v int)").unwrap();
    e.execute("INSERT INTO w VALUES (1,10),(2,20)").unwrap();
    e.execute("CREATE RULE rw AS ON UPDATE TO w WHERE OLD.id = 1 DO INSTEAD NOTHING")
        .unwrap();
    assert_eq!(affected(&mut e, "UPDATE w SET v = 0"), 1);
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM w ORDER BY id"),
        vec![(1, Some(10)), (2, Some(0))]
    );
}

#[test]
fn insert_conditional_multi_row() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ins(id int, v int)").unwrap();
    e.execute("CREATE RULE ri AS ON INSERT TO ins WHERE NEW.v > 15 DO INSTEAD NOTHING")
        .unwrap();
    // Only v <= 15 rows survive.
    assert_eq!(affected(&mut e, "INSERT INTO ins VALUES (1,10),(2,20),(3,30)"), 1);
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM ins ORDER BY id"),
        vec![(1, Some(10))]
    );
}

#[test]
fn conditional_null_means_rule_does_not_apply() {
    // NEW.v < 0 evaluates to NULL for a NULL v → rule does not apply → row is
    // affected (PG semantics).
    let mut e = Engine::new();
    e.execute("CREATE TABLE u(id int, v int)").unwrap();
    e.execute("INSERT INTO u VALUES (1, NULL), (2, 7)").unwrap();
    e.execute("CREATE RULE ru AS ON UPDATE TO u WHERE NEW.v < 0 DO INSTEAD NOTHING")
        .unwrap();
    assert_eq!(affected(&mut e, "UPDATE u SET v = v"), 2);
    // Same for INSERT: a NULL-v row is not blocked.
    e.execute("CREATE TABLE ins(id int, v int)").unwrap();
    e.execute("CREATE RULE ri AS ON INSERT TO ins WHERE NEW.v < 0 DO INSTEAD NOTHING")
        .unwrap();
    assert_eq!(affected(&mut e, "INSERT INTO ins VALUES (1, NULL), (2, -3), (3, 8)"), 2);
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM ins ORDER BY id"),
        vec![(1, None), (3, Some(8))]
    );
}
