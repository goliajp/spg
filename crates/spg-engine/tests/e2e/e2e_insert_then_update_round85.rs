//! v7.39 (read01 round 85) — INSERT then UPDATE/DELETE of the same row in one
//! transaction lost the row.
//!
//! A differential MVCC sweep found (embedded AND over the wire):
//!
//!     BEGIN;
//!     INSERT INTO t VALUES (6, 60);
//!     UPDATE t SET v = 99 WHERE id = 6;   -- ok
//!     SELECT v FROM t WHERE id = 6;        -- ERROR: duplicate key ...
//!
//! The UPDATE tombstones the just-inserted row and appends a new version, so the
//! transaction's write-set held BOTH the original insert `{6,60}` and the
//! update's new insert `{6,99}` with the same primary key. The uniqueness
//! pre-check that runs on every subsequent statement (the RC rebase) then saw
//! two staged inserts with key 6 and failed with a spurious duplicate-key error
//! — and on COMMIT the row was lost.
//!
//! The original insert `{6,60}` is a PHANTOM: this same transaction tombstoned
//! it (an "own cycle" insert-then-tombstone), so it never becomes visible and
//! must not enter the uniqueness pre-check. Excluding it leaves only the
//! surviving `{6,99}`, which is unique. A genuine duplicate (two live inserts of
//! the same key, neither tombstoned) is untouched and still errors.

use spg_engine::{Engine, QueryResult};

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => {
            if rows.is_empty() {
                "<none>".to_string()
            } else {
                spg_engine::eval::value_to_text(&rows[0].values[0])
            }
        }
        other => panic!("{sql}: {other:?}"),
    }
}

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"));
}

#[test]
fn a_insert_then_update_same_row() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE t (id int primary key, v int)");
    ok(&mut e, "BEGIN");
    ok(&mut e, "INSERT INTO t VALUES (6, 60)");
    ok(&mut e, "UPDATE t SET v = 99 WHERE id = 6");
    // Previously this SELECT tripped a spurious duplicate-key error.
    assert_eq!(r1(&mut e, "SELECT v FROM t WHERE id = 6"), "99");
    assert_eq!(r1(&mut e, "SELECT count(*) FROM t"), "1");
    ok(&mut e, "COMMIT");
    // The row survives the commit with the updated value.
    assert_eq!(r1(&mut e, "SELECT v FROM t WHERE id = 6"), "99");
    assert_eq!(r1(&mut e, "SELECT count(*) FROM t"), "1");
}

#[test]
fn b_insert_then_update_twice() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE t (id int primary key, v int)");
    ok(&mut e, "BEGIN");
    ok(&mut e, "INSERT INTO t VALUES (2, 20)");
    ok(&mut e, "UPDATE t SET v = 21 WHERE id = 2");
    ok(&mut e, "UPDATE t SET v = 22 WHERE id = 2");
    assert_eq!(r1(&mut e, "SELECT v FROM t WHERE id = 2"), "22");
    ok(&mut e, "COMMIT");
    assert_eq!(r1(&mut e, "SELECT v FROM t WHERE id = 2"), "22");
}

#[test]
fn c_insert_then_delete_then_reinsert() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE t (id int primary key, v int)");
    ok(&mut e, "BEGIN");
    ok(&mut e, "INSERT INTO t VALUES (5, 50)");
    ok(&mut e, "DELETE FROM t WHERE id = 5");
    assert_eq!(r1(&mut e, "SELECT count(*) FROM t WHERE id = 5"), "0");
    // Re-inserting the key after deleting the same-tx insert works.
    ok(&mut e, "INSERT INTO t VALUES (5, 55)");
    assert_eq!(r1(&mut e, "SELECT v FROM t WHERE id = 5"), "55");
    ok(&mut e, "COMMIT");
    assert_eq!(r1(&mut e, "SELECT v FROM t WHERE id = 5"), "55");
}

#[test]
fn d_genuine_duplicate_insert_still_errors() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE t (id int primary key, v int)");
    ok(&mut e, "BEGIN");
    ok(&mut e, "INSERT INTO t VALUES (9, 90)");
    // Two LIVE inserts of the same key (neither tombstoned) — the fix must not
    // mask this; it stays a duplicate-key error.
    assert!(e.execute("INSERT INTO t VALUES (9, 91)").is_err());
    ok(&mut e, "ROLLBACK");
    assert_eq!(r1(&mut e, "SELECT count(*) FROM t WHERE id = 9"), "0");
}

#[test]
fn e_update_committed_row_and_insert_new() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE t (id int primary key, v int)");
    ok(&mut e, "INSERT INTO t VALUES (1, 10)");
    ok(&mut e, "BEGIN");
    ok(&mut e, "UPDATE t SET v = 11 WHERE id = 1");
    ok(&mut e, "INSERT INTO t VALUES (3, 30)");
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(id::text || '=' || v::text, ',' ORDER BY id) FROM t"
        ),
        "1=11,3=30"
    );
    ok(&mut e, "COMMIT");
    assert_eq!(r1(&mut e, "SELECT count(*) FROM t"), "2");
}
