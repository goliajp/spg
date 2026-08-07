//! read01 round 347 (MySQL differential, M2) — LAST_INSERT_ID() answers.
//!
//! It was hard-coded to 0. A client that inserts a row and then uses the
//! id as the parent key for the next insert wrote a **0** — silently,
//! because 0 is a perfectly legal integer. That is the whole point of the
//! function, and it is the single most common way MySQL code links two
//! inserts.
//!
//! MariaDB 11, measured, in this order: a fresh session reads 0; a
//! single-row insert sets it to the generated id; a THREE-row insert sets
//! it to the FIRST of the three, not the last; and a statement that
//! generates no AUTO_INCREMENT value — an explicit id, an UPDATE, a
//! DELETE, an insert into a table that has no such column — leaves it
//! alone. `LAST_INSERT_ID(42)` answers 42 and sets it for later calls.
//!
//! It is per SESSION, kept in the session bag from the first commit:
//! rounds 277/279/283 each paid for landing per-connection state on the
//! shared engine first, and this one never gets a process-wide version to
//! regress from.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn scalar(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
            .unwrap_or(Value::Null),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn last_id(e: &mut Engine) -> i64 {
    match scalar(e, "SELECT LAST_INSERT_ID()") {
        Value::BigInt(n) => n,
        other => panic!("{other:?}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e.execute("CREATE TABLE li (id INT AUTO_INCREMENT PRIMARY KEY, n INT)")
        .unwrap();
    e.execute("CREATE TABLE plain (a INT)").unwrap();
    e
}

/// The measured sequence, value for value.
#[test]
fn it_follows_mariadbs_sequence() {
    let mut e = fixture();
    assert_eq!(last_id(&mut e), 0, "a fresh session");

    e.execute("INSERT INTO li (n) VALUES (10)").unwrap();
    assert_eq!(last_id(&mut e), 1, "the generated id");

    e.execute("INSERT INTO li (n) VALUES (20),(30),(40)")
        .unwrap();
    assert_eq!(last_id(&mut e), 2, "the FIRST of the three, not the last");

    e.execute("INSERT INTO plain VALUES (1)").unwrap();
    assert_eq!(last_id(&mut e), 2, "a table with no AUTO_INCREMENT");

    e.execute("UPDATE li SET n = 99 WHERE id = 1").unwrap();
    assert_eq!(last_id(&mut e), 2, "an UPDATE");

    e.execute("INSERT INTO li (id,n) VALUES (100,1)").unwrap();
    assert_eq!(last_id(&mut e), 2, "an explicit id generates nothing");

    e.execute("DELETE FROM li").unwrap();
    assert_eq!(last_id(&mut e), 2, "a DELETE");
}

/// One argument sets it — MySQL's own way of parking a value in the
/// session — and every later bare call reads it back.
#[test]
fn one_argument_sets_it() {
    let mut e = fixture();
    e.execute("INSERT INTO li (n) VALUES (1)").unwrap();
    assert_eq!(last_id(&mut e), 1);
    assert_eq!(
        scalar(&mut e, "SELECT LAST_INSERT_ID(42)"),
        Value::BigInt(42)
    );
    assert_eq!(last_id(&mut e), 42, "the argument stuck");
    assert_eq!(scalar(&mut e, "SELECT 1"), Value::Int(1));
    assert_eq!(last_id(&mut e), 42, "an ordinary SELECT leaves it alone");
}

/// The id a client links its inserts with is really the row's.
#[test]
fn the_value_is_the_row_that_was_inserted() {
    let mut e = fixture();
    e.execute("INSERT INTO li (n) VALUES (7)").unwrap();
    let id = last_id(&mut e);
    assert_eq!(
        scalar(&mut e, &format!("SELECT n FROM li WHERE id = {id}")),
        Value::Int(7),
    );
}

/// Per connection, not per process — one session's insert is invisible to
/// another's, and the first one's value survives the round trip.
#[test]
fn it_is_session_state() {
    let mut e = fixture();
    e.set_current_session(1);
    e.execute("INSERT INTO li (n) VALUES (10)").unwrap();
    let first = last_id(&mut e);
    assert!(first > 0);

    e.set_current_session(2);
    assert_eq!(last_id(&mut e), 0, "a second connection starts at 0");
    e.execute("INSERT INTO li (n) VALUES (20)").unwrap();
    let second = last_id(&mut e);
    assert_ne!(second, first);

    e.set_current_session(1);
    assert_eq!(
        last_id(&mut e),
        first,
        "the first connection's value is its own"
    );
}
