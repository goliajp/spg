//! 8.0.3 — a DO block is one statement, and a statement that fails
//! leaves nothing behind.
//!
//! The body's writes landed one at a time, so a body whose second write
//! failed kept its first: visible to every later statement, and absent
//! from the WAL, which records a statement only when it succeeds. Found
//! while measuring sentori's report that an `EXCEPTION` handler does not
//! catch a unique violation. One row (1) present, then
//! `DO $$ BEGIN INSERT (2); INSERT (1); END $$`:
//!
//! ```text
//!   SPG, before a restart   1,2
//!   SPG, after kill -9      1
//!   PG 18.6                 1
//! ```

use spg_engine::{Engine, QueryResult};

fn keys(e: &mut Engine) -> String {
    match e
        .execute("SELECT string_agg(k::text, ',' ORDER BY k) FROM da")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    }
}

fn boot() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE da (k int UNIQUE)").unwrap();
    e.execute("INSERT INTO da VALUES (1)").unwrap();
    e
}

#[test]
fn a_do_whose_second_write_fails_keeps_neither() {
    let mut e = boot();
    let err = e
        .execute("DO $$ BEGIN INSERT INTO da VALUES (2); INSERT INTO da VALUES (1); END $$")
        .expect_err("the second insert is a duplicate");
    assert!(format!("{err}").contains("duplicate key"), "{err}");
    assert_eq!(keys(&mut e), "1", "PG 18.6 leaves the table as it was");
}

#[test]
fn a_do_that_succeeds_keeps_every_write() {
    let mut e = boot();
    e.execute("DO $$ BEGIN INSERT INTO da VALUES (3); INSERT INTO da VALUES (4); END $$")
        .unwrap();
    assert_eq!(keys(&mut e), "1,3,4");
}

/// Statements before and after a failed DO are their own statements.
#[test]
fn the_statements_around_a_failed_do_are_unaffected() {
    let mut e = boot();
    e.execute("INSERT INTO da VALUES (5)").unwrap();
    assert!(
        e.execute("DO $$ BEGIN INSERT INTO da VALUES (6); INSERT INTO da VALUES (5); END $$")
            .is_err()
    );
    e.execute("INSERT INTO da VALUES (7)").unwrap();
    assert_eq!(keys(&mut e), "1,5,7");
}
