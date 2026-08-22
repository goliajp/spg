//! v7.38.18 (S10) — `ANALYZE` could not see a table created earlier in
//! the same simple-query string.
//!
//!     CREATE TABLE t (k INT); INSERT INTO t VALUES (1); ANALYZE t;
//!
//! sent as ONE string answered `relation "t" does not exist`, while the
//! INSERT in that same string had just succeeded and PostgreSQL 18.4
//! answers `ANALYZE`.
//!
//! A multi-statement simple query is an implicit transaction, so the new
//! table lives in the transaction's shadow catalog; `exec_analyze` read
//! the COMMITTED catalog alone, in four places.
//!
//! Seven other statement kinds in that position were already right —
//! SELECT, UPDATE, DELETE, CREATE INDEX, ALTER TABLE, TRUNCATE, DROP —
//! which is why it looked like a quirk of ANALYZE rather than a class.
//! Measured for each of the eight, with a FRESH table name per case:
//! reusing one name hid the defect completely, because the second run
//! found the table already committed.
//!
//! In-process, an `Engine::execute` call is one statement, so this test
//! is where the shape can be reproduced at all.

use spg_engine::{Engine, QueryResult};

fn count(e: &mut Engine, sql: &str) -> i64 {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => match rows.first().map(|r| r.values[0].clone()) {
            Some(spg_storage::Value::BigInt(n)) => n,
            Some(spg_storage::Value::Int(n)) => i64::from(n),
            other => panic!("{sql}: {other:?}"),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

/// The reported shape: inside an explicit transaction, which is what a
/// multi-statement simple query becomes.
#[test]
fn analyze_sees_a_table_created_in_the_same_transaction() {
    let mut e = Engine::new();
    e.execute("BEGIN").unwrap();
    e.execute("CREATE TABLE fresh (k INT, s TEXT)").unwrap();
    e.execute("INSERT INTO fresh VALUES (1,'a'),(2,'b')")
        .unwrap();

    // This is the call that answered `relation "fresh" does not exist`.
    e.execute("ANALYZE fresh")
        .expect("ANALYZE must see a table created in this transaction");

    // And it must have done the WORK. "It returned OK" is not evidence:
    // two columns analysed is two rows of statistics.
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM pg_statistic WHERE starelid = \
             (SELECT oid FROM pg_class WHERE relname = 'fresh')"
        ),
        2,
        "ANALYZE returned OK without populating anything"
    );
    e.execute("COMMIT").unwrap();
}

/// A bare `ANALYZE` covers the tables this transaction created too — the
/// same defect at the site that lists them rather than the one that
/// checks a name.
#[test]
fn bare_analyze_covers_a_table_created_in_the_same_transaction() {
    let mut e = Engine::new();
    e.execute("BEGIN").unwrap();
    e.execute("CREATE TABLE bare_fresh (k INT)").unwrap();
    e.execute("INSERT INTO bare_fresh VALUES (1)").unwrap();
    e.execute("ANALYZE").unwrap();
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM pg_statistic WHERE starelid = \
             (SELECT oid FROM pg_class WHERE relname = 'bare_fresh')"
        ),
        1,
        "a bare ANALYZE skipped a table this transaction created"
    );
    e.execute("COMMIT").unwrap();
}
