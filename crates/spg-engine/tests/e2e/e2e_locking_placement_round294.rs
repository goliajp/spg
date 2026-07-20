//! v7.39 (round 294, E3 Phase 1b-i) — where a row-locking clause is
//! allowed to appear.
//!
//! PG refuses `FOR UPDATE` on exactly the shapes that have no
//! identifiable base row to lock, each with its own wording. SPG
//! accepted every one of them and locked nothing, so a query PG refuses
//! outright came back looking like it had taken locks.
//!
//! That correspondence is not a coincidence and it matters for the rest
//! of the epic: the shapes PG rejects are precisely the ones where SPG
//! could not carry a row identity through the projection either. Making
//! them error first shrinks the surface scan-time locking has to cover.
//!
//! `FOR UPDATE OF t` is validated against the FROM clause, which also
//! forced the parser to CAPTURE the OF list — it was being consumed and
//! dropped, and an uncaptured list silently reads as "lock everything".
//!
//! Every wording read off live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}").replace("unsupported: ", ""),
    }
}

fn ok(e: &mut Engine, sql: &str) -> usize {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    rows.len()
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE s1 (id int primary key, v int)").unwrap();
    e.execute("CREATE TABLE s2 (id int primary key, w int)").unwrap();
    e.execute("INSERT INTO s1 VALUES (1,10),(2,20)").unwrap();
    e.execute("INSERT INTO s2 VALUES (1,100)").unwrap();
    e
}

#[test]
fn the_four_disallowed_shapes_carry_pgs_wordings() {
    let mut e = fixture();
    assert_eq!(
        err(&mut e, "SELECT count(*) FROM s1 FOR UPDATE"),
        "FOR UPDATE is not allowed with aggregate functions",
    );
    assert_eq!(
        err(&mut e, "SELECT DISTINCT v FROM s1 FOR UPDATE"),
        "FOR UPDATE is not allowed with DISTINCT clause",
    );
    assert_eq!(
        err(&mut e, "SELECT v FROM s1 GROUP BY v FOR UPDATE"),
        "FOR UPDATE is not allowed with GROUP BY clause",
    );
    assert_eq!(
        err(&mut e, "SELECT v FROM s1 UNION SELECT w FROM s2 FOR UPDATE"),
        "FOR UPDATE is not allowed with UNION/INTERSECT/EXCEPT",
    );
}

#[test]
fn the_wording_names_the_strength_that_was_asked_for() {
    // PG echoes the clause the query used, not a generic "FOR UPDATE".
    let mut e = fixture();
    assert_eq!(
        err(&mut e, "SELECT count(*) FROM s1 FOR SHARE"),
        "FOR SHARE is not allowed with aggregate functions",
    );
    assert_eq!(
        err(&mut e, "SELECT DISTINCT v FROM s1 FOR KEY SHARE"),
        "FOR KEY SHARE is not allowed with DISTINCT clause",
    );
    assert_eq!(
        err(&mut e, "SELECT count(*) FROM s1 FOR NO KEY UPDATE"),
        "FOR NO KEY UPDATE is not allowed with aggregate functions",
    );
}

#[test]
fn of_must_name_a_relation_from_the_from_clause() {
    let mut e = fixture();
    assert_eq!(
        err(&mut e, "SELECT * FROM s1 FOR UPDATE OF nosuch"),
        "relation \"nosuch\" in FOR UPDATE clause not found in FROM clause",
    );
    // A real one passes — the list is genuinely captured, not ignored.
    assert_eq!(ok(&mut e, "SELECT * FROM s1 FOR UPDATE OF s1"), 2);
    assert_eq!(
        ok(&mut e, "SELECT * FROM s1 JOIN s2 USING (id) FOR UPDATE OF s1"),
        1,
    );
}

#[test]
fn the_allowed_shapes_still_run() {
    // PG permits these; refusing them would be a capability regression.
    let mut e = fixture();
    assert_eq!(ok(&mut e, "SELECT * FROM s1 FOR UPDATE"), 2);
    assert_eq!(ok(&mut e, "SELECT * FROM s1 ORDER BY id FOR UPDATE"), 2);
    assert_eq!(ok(&mut e, "SELECT * FROM s1 LIMIT 1 FOR UPDATE"), 1);
    assert_eq!(ok(&mut e, "SELECT 1 FOR UPDATE"), 1);
    assert_eq!(ok(&mut e, "SELECT * FROM (SELECT * FROM s1) x FOR UPDATE"), 2);
    assert_eq!(
        ok(&mut e, "WITH c AS (SELECT * FROM s1) SELECT * FROM c FOR UPDATE"),
        2,
    );
    assert_eq!(ok(&mut e, "SELECT * FROM s1 JOIN s2 USING (id) FOR UPDATE"), 1);
}

#[test]
fn a_query_without_the_clause_is_unaffected() {
    let mut e = fixture();
    assert_eq!(ok(&mut e, "SELECT count(*) FROM s1"), 1);
    assert_eq!(ok(&mut e, "SELECT DISTINCT v FROM s1"), 2);
    assert_eq!(ok(&mut e, "SELECT v FROM s1 UNION SELECT w FROM s2"), 3);
}

#[test]
fn a_join_is_accepted_but_says_it_did_not_lock() {
    // v7.39 (round 298) — PG locks the base rows of a join; SPG cannot
    // yet name which relation each result row came from.
    //
    // Three options, each costing something: refuse (a capability
    // regression on SQL PG accepts), lock nothing silently (the exact
    // failure this epic exists to remove), or support it (a separate
    // slice). The choice is to accept, return the rows, and TELL the
    // client — an announced gap rather than a hidden one.
    let mut e = fixture();
    let r = e
        .execute("SELECT * FROM s1 JOIN s2 USING (id) FOR UPDATE")
        .expect("PG accepts this shape, so SPG must too");
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    assert_eq!(rows.len(), 1);
    let notices = e.take_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("NOT enforced") && notices[0].starts_with("FOR UPDATE"),
        "{notices:?}",
    );
}

#[test]
fn a_single_table_lock_says_nothing() {
    // The announcement must be specific to the unsupported shape — a
    // notice on every locking SELECT would be noise, and would train
    // operators to ignore it.
    let mut e = fixture();
    e.execute("SELECT * FROM s1 FOR UPDATE").unwrap();
    assert!(e.take_notices().is_empty());
}
