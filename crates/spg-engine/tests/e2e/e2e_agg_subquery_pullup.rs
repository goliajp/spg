//! Sublink pull-up: `AGG((SELECT col FROM t WHERE t.key = outer.x))` with a
//! UNIQUE/PK key rewrites to a LEFT JOIN (v7.33, mailrs 7.32.1). These pin
//! that the rewrite is SEMANTICS-PRESERVING — identical to both the per-row
//! subquery and a hand-written LEFT JOIN — across no-match NULLs, multiple
//! aggregates, inner predicates, and the non-unique key case where the
//! rewrite must NOT fire.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(e: &mut Engine, sql: &str) -> Vec<spg_storage::Row> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from {sql:?}, got {other:?}"),
    }
}

fn setup_unique() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE o (id INT, grp INT)").unwrap();
    e.execute("CREATE TABLE i (k INT PRIMARY KEY, v INT, flag BOOL)")
        .unwrap();
    // grp 10 = ids 1,2 (both matched); grp 20 = id 3 (NO inner match).
    e.execute("INSERT INTO o (id, grp) VALUES (1,10),(2,10),(3,20)")
        .unwrap();
    e.execute("INSERT INTO i (k, v, flag) VALUES (1,100,true),(2,200,false)")
        .unwrap();
    e
}

#[test]
fn pullup_bool_or_and_max_with_no_match_nulls() {
    let mut e = setup_unique();
    let rows = rows_of(
        &mut e,
        "SELECT o.grp, \
            BOOL_OR((SELECT i.flag FROM i WHERE i.k = o.id)), \
            MAX((SELECT i.v FROM i WHERE i.k = o.id)) \
         FROM o GROUP BY o.grp ORDER BY o.grp",
    );
    assert_eq!(rows.len(), 2);
    // grp 10: BOOL_OR(true,false)=true; MAX(100,200)=200.
    assert!(
        matches!(rows[0].values[1], Value::Bool(true)),
        "grp10 bool_or"
    );
    assert!(matches!(rows[0].values[2], Value::Int(200)), "grp10 max");
    // grp 20: id 3 has no inner row → subquery NULL → BOOL_OR/MAX over a
    // single NULL is NULL.
    assert!(
        matches!(rows[1].values[1], Value::Null),
        "grp20 bool_or null"
    );
    assert!(matches!(rows[1].values[2], Value::Null), "grp20 max null");
}

#[test]
fn pullup_matches_hand_written_left_join() {
    // Differential: the correlated-subquery form (which the planner rewrites
    // to a LEFT JOIN) must equal the explicit LEFT JOIN form, row for row.
    let mut e = setup_unique();
    let subq = rows_of(
        &mut e,
        "SELECT o.grp, BOOL_OR((SELECT i.flag FROM i WHERE i.k = o.id)) \
         FROM o GROUP BY o.grp ORDER BY o.grp",
    );
    let join = rows_of(
        &mut e,
        "SELECT o.grp, BOOL_OR(j.flag) \
         FROM o LEFT JOIN i j ON j.k = o.id GROUP BY o.grp ORDER BY o.grp",
    );
    assert_eq!(subq.len(), join.len());
    for (a, b) in subq.iter().zip(join.iter()) {
        assert_eq!(a.values, b.values, "subquery vs hand-written join");
    }
}

#[test]
fn pullup_carries_inner_predicate_into_on() {
    // An extra all-inner predicate must survive into the join ON. Only
    // inner rows with v >= 150 count: id1 (v=100) is filtered out, so its
    // flag no longer contributes.
    let mut e = setup_unique();
    let rows = rows_of(
        &mut e,
        "SELECT o.grp, BOOL_OR((SELECT i.flag FROM i WHERE i.k = o.id AND i.v >= 150)) \
         FROM o GROUP BY o.grp ORDER BY o.grp",
    );
    // grp 10: id1 (v=100) filtered → NULL; id2 (v=200,flag=false) → false.
    // BOOL_OR(NULL, false) = false.
    assert!(
        matches!(rows[0].values[1], Value::Bool(false)),
        "grp10 filtered bool_or"
    );
    // grp 20: still no match → NULL.
    assert!(matches!(rows[1].values[1], Value::Null), "grp20 null");
}

#[test]
fn non_unique_key_still_correct() {
    // No UNIQUE/PK on the key → the pull-up must NOT fire (a LEFT JOIN could
    // multiply rows). The per-row path stays and the answer is still right.
    // Data keeps at most one match per key so the scalar subquery is valid.
    let mut e = Engine::new();
    e.execute("CREATE TABLE o (id INT, grp INT)").unwrap();
    e.execute("CREATE TABLE i (k INT, v INT)").unwrap(); // no constraint on k
    e.execute("INSERT INTO o (id, grp) VALUES (1,10),(2,10),(3,20)")
        .unwrap();
    e.execute("INSERT INTO i (k, v) VALUES (1,100),(2,200),(3,300)")
        .unwrap();
    let rows = rows_of(
        &mut e,
        "SELECT o.grp, SUM((SELECT i.v FROM i WHERE i.k = o.id)) \
         FROM o GROUP BY o.grp ORDER BY o.grp",
    );
    assert_eq!(rows.len(), 2);
    // grp 10: 100+200=300; grp 20: 300.
    assert!(
        matches!(&rows[0].values[1], Value::BigInt(300) | Value::Int(300)),
        "grp10 sum {:?}",
        rows[0].values[1]
    );
    assert!(
        matches!(&rows[1].values[1], Value::BigInt(300) | Value::Int(300)),
        "grp20 sum {:?}",
        rows[1].values[1]
    );
}

#[test]
fn pullup_two_subqueries_one_query() {
    // Two distinct correlated subqueries → two LEFT JOINs, each on its own
    // fresh alias. Both must resolve independently.
    let mut e = setup_unique();
    e.execute("CREATE TABLE i2 (k INT PRIMARY KEY, label TEXT)")
        .unwrap();
    e.execute("INSERT INTO i2 (k, label) VALUES (1,'a'),(2,'b'),(3,'c')")
        .unwrap();
    let rows = rows_of(
        &mut e,
        "SELECT o.grp, \
            MAX((SELECT i.v FROM i WHERE i.k = o.id)), \
            MAX((SELECT i2.label FROM i2 WHERE i2.k = o.id)) \
         FROM o GROUP BY o.grp ORDER BY o.grp",
    );
    assert_eq!(rows.len(), 2);
    assert!(matches!(rows[0].values[1], Value::Int(200)), "grp10 max v");
    // grp 10 labels a,b → MAX = 'b'.
    assert_eq!(
        match &rows[0].values[2] {
            Value::Text(s) => s.as_str(),
            o => panic!("{o:?}"),
        },
        "b"
    );
    // grp 20 id3 → v has no match (NULL), label 'c' exists.
    assert!(matches!(rows[1].values[1], Value::Null), "grp20 v null");
    assert_eq!(
        match &rows[1].values[2] {
            Value::Text(s) => s.as_str(),
            o => panic!("{o:?}"),
        },
        "c"
    );
}
