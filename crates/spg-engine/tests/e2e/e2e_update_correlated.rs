//! mailrs round-28 — correlated scalar subquery in UPDATE SET / WHERE.
//! `UPDATE t SET c = (SELECT … WHERE … = t.x)` must bind the target
//! row as the subquery's outer context (the non-correlated form
//! already worked). Verbatim repro from the report + the WHERE/sweep.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn run(e: &mut Engine, sql: &str) -> QueryResult {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
}

fn scalar(e: &mut Engine, sql: &str) -> i64 {
    match run(e, sql) {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            Value::Int(n) => i64::from(n),
            Value::BigInt(n) => n,
            ref o => panic!("not int: {o:?}"),
        },
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn seed() -> Engine {
    let mut e = Engine::new();
    run(
        &mut e,
        "CREATE TABLE mb (id BIGINT PRIMARY KEY, uidnext INT)",
    );
    run(&mut e, "CREATE TABLE msg (mailbox_id BIGINT, uid INT)");
    run(&mut e, "INSERT INTO mb VALUES (1, 5), (2, 5)");
    run(
        &mut e,
        "INSERT INTO msg VALUES (1, 10), (1, 12), (2, 99), (2, 7)",
    );
    e
}

#[test]
fn correlated_scalar_subquery_in_set() {
    let mut e = seed();
    // The report's repro (a): per-mailbox MAX over the child table.
    run(
        &mut e,
        "UPDATE mb SET uidnext = (SELECT MAX(m.uid) FROM msg m WHERE m.mailbox_id = mb.id)",
    );
    assert_eq!(scalar(&mut e, "SELECT uidnext FROM mb WHERE id = 1"), 12);
    assert_eq!(scalar(&mut e, "SELECT uidnext FROM mb WHERE id = 2"), 99);
}

#[test]
fn noncorrelated_subquery_in_set_still_works() {
    let mut e = seed();
    // The report's repro (b): regression guard.
    run(&mut e, "UPDATE mb SET uidnext = (SELECT MAX(uid) FROM msg)");
    assert_eq!(scalar(&mut e, "SELECT uidnext FROM mb WHERE id = 1"), 99);
    assert_eq!(scalar(&mut e, "SELECT uidnext FROM mb WHERE id = 2"), 99);
}

#[test]
fn correlated_subquery_in_update_where() {
    let mut e = seed();
    // Sweep: correlated subquery in the UPDATE WHERE clause.
    run(
        &mut e,
        "UPDATE mb SET uidnext = 1 \
         WHERE (SELECT MAX(m.uid) FROM msg m WHERE m.mailbox_id = mb.id) > 50",
    );
    // Only mailbox 2 (max 99 > 50) is reset.
    assert_eq!(scalar(&mut e, "SELECT uidnext FROM mb WHERE id = 1"), 5);
    assert_eq!(scalar(&mut e, "SELECT uidnext FROM mb WHERE id = 2"), 1);
}

#[test]
fn correlated_set_plus_where_together() {
    let mut e = seed();
    run(
        &mut e,
        "UPDATE mb SET uidnext = (SELECT MAX(m.uid) + 1 FROM msg m WHERE m.mailbox_id = mb.id) \
         WHERE (SELECT COUNT(*) FROM msg m WHERE m.mailbox_id = mb.id) >= 2",
    );
    // Both mailboxes have 2 messages → both updated to per-box max+1.
    assert_eq!(scalar(&mut e, "SELECT uidnext FROM mb WHERE id = 1"), 13);
    assert_eq!(scalar(&mut e, "SELECT uidnext FROM mb WHERE id = 2"), 100);
}
