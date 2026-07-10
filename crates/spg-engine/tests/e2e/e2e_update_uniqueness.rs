//! v7.38 (read01 U1) — UPDATE enforces UNIQUE / PRIMARY KEY and unique
//! indexes (plain + expression + partial), matching PG's non-deferrable
//! immediate semantics.
//!
//! Before this, the UPDATE path checked FK / CHECK / NOT NULL but silently
//! skipped every uniqueness check, so an UPDATE could move a row onto a key
//! another row already held. All expected results are live-PG18.4-verified.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}
fn err(e: &mut Engine, sql: &str) {
    assert!(e.execute(sql).is_err(), "{sql} should have been rejected");
}
fn count(e: &mut Engine, sql: &str) -> usize {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn update_to_duplicate_unique_constraint_is_rejected() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE u(x int UNIQUE)");
    ok(&mut e, "INSERT INTO u VALUES(1),(2)");
    err(&mut e, "UPDATE u SET x=1 WHERE x=2");
    // Row 2 keeps its old value.
    assert_eq!(count(&mut e, "SELECT * FROM u WHERE x=2"), 1);
}

#[test]
fn update_to_duplicate_plain_unique_index_is_rejected() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE u(x int)");
    ok(&mut e, "CREATE UNIQUE INDEX ux ON u(x)");
    ok(&mut e, "INSERT INTO u VALUES(1),(2)");
    err(&mut e, "UPDATE u SET x=1 WHERE x=2");
}

#[test]
fn update_to_duplicate_expression_unique_index_is_rejected() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE u(email text, note text)");
    ok(&mut e, "CREATE UNIQUE INDEX u_le ON u(lower(email))");
    ok(
        &mut e,
        "INSERT INTO u VALUES('A@x.com','a'),('b@y.com','b')",
    );
    // Colliding on lower(email) is rejected...
    err(&mut e, "UPDATE u SET email='A@X.COM' WHERE email='b@y.com'");
    // ...a genuinely new key is fine...
    ok(&mut e, "UPDATE u SET email='c@z.com' WHERE email='b@y.com'");
    // ...and touching a non-key column never trips the check.
    ok(&mut e, "UPDATE u SET note='z' WHERE email='A@x.com'");
}

#[test]
fn update_no_op_and_free_reassign_pass() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE u(x int)");
    ok(&mut e, "CREATE UNIQUE INDEX ux ON u(x)");
    ok(&mut e, "INSERT INTO u VALUES(1),(2),(3)");
    ok(&mut e, "UPDATE u SET x=x"); // key unchanged — no self-collision
    ok(&mut e, "UPDATE u SET x=x+10"); // shifts clear of every existing key
    assert_eq!(count(&mut e, "SELECT * FROM u"), 3);
}

#[test]
fn update_swap_and_shift_are_rejected_like_pg() {
    // PG's non-deferrable unique constraint rejects a swap and an
    // adjacent shift because of the transient duplicate mid-statement.
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE u(x int)");
    ok(&mut e, "CREATE UNIQUE INDEX ux ON u(x)");
    ok(&mut e, "INSERT INTO u VALUES(1),(2),(3)");
    err(
        &mut e,
        "UPDATE u SET x=CASE WHEN x=1 THEN 2 WHEN x=2 THEN 1 ELSE x END",
    );
    err(&mut e, "UPDATE u SET x=x+1");
}

#[test]
fn update_unique_column_to_null_allows_multiple() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE u(x int UNIQUE)");
    ok(&mut e, "INSERT INTO u VALUES(1),(2)");
    // NULLs are distinct under a plain UNIQUE, so two NULLs coexist.
    ok(&mut e, "UPDATE u SET x=NULL WHERE x=1");
    ok(&mut e, "UPDATE u SET x=NULL WHERE x=2");
    assert_eq!(count(&mut e, "SELECT * FROM u WHERE x IS NULL"), 2);
}
