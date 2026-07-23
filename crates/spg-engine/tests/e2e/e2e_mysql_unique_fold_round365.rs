//! read01 round 365 (MySQL differential, M4 P3) — UNIQUE / PRIMARY KEY
//! and the index write path fold under the default collation, so the
//! write path rejects exactly the duplicates the read path (P2, r364)
//! already collapses.
//!
//! MariaDB 11's default `utf8mb4_uca1400_ai_ci` is case- AND accent-
//! insensitive, so a second row whose text folds to an existing key is a
//! `1062 Duplicate entry` — `'A'` after `'a'`, `'Bär'` after `'bar'`,
//! `'ß'` after `'ss'`. SPG used to accept all of these (byte-wise
//! uniqueness), which after P2 was self-contradictory: `SELECT DISTINCT`
//! folded to one group while the table physically held both rows. A
//! byte-typed column (`VARBINARY`) still keeps both, matching MariaDB,
//! because it stores bytes, not text. A PostgreSQL session is
//! case-sensitive and unaffected.
//!
//! Every expectation is copied from a MariaDB 11 run of the same
//! statements.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("`{sql}` should have succeeded: {err}"));
}

fn dup(e: &mut Engine, sql: &str) {
    let r = e.execute(sql);
    assert!(
        r.is_err(),
        "`{sql}` should have been rejected as a duplicate, got {r:?}"
    );
}

fn count(e: &mut Engine, sql: &str) -> i64 {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            spg_storage::Value::BigInt(n) => n,
            ref other => panic!("expected BigInt count, got {other:?}"),
        },
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// A single-column UNIQUE folds case, accent and expansion; a genuinely
/// distinct value still inserts.
#[test]
fn unique_column_folds_case_accent_expansion() {
    let mut e = mysql();
    ok(&mut e, "CREATE TABLE u (t VARCHAR(10) UNIQUE)");
    ok(&mut e, "INSERT INTO u VALUES ('a')");
    dup(&mut e, "INSERT INTO u VALUES ('A')"); // case
    ok(&mut e, "INSERT INTO u VALUES ('bar')");
    dup(&mut e, "INSERT INTO u VALUES ('Bär')"); // accent
    ok(&mut e, "INSERT INTO u VALUES ('ss')");
    dup(&mut e, "INSERT INTO u VALUES ('ß')"); // expansion ß→ss
    ok(&mut e, "INSERT INTO u VALUES ('baz')"); // genuinely new
    // {a, bar, ss, baz} survive — MariaDB kept exactly these.
    assert_eq!(count(&mut e, "SELECT COUNT(*) FROM u"), 4);
}

/// A batch INSERT with two fold-equal rows is rejected as a whole.
#[test]
fn batch_insert_catches_intra_batch_fold_dup() {
    let mut e = mysql();
    ok(&mut e, "CREATE TABLE u (t VARCHAR(10) UNIQUE)");
    dup(&mut e, "INSERT INTO u VALUES ('foo'),('FOO')");
    // Nothing was committed.
    assert_eq!(count(&mut e, "SELECT COUNT(*) FROM u"), 0);
}

/// PRIMARY KEY folds the same way.
#[test]
fn primary_key_folds() {
    let mut e = mysql();
    ok(&mut e, "CREATE TABLE pk (t VARCHAR(10) PRIMARY KEY)");
    ok(&mut e, "INSERT INTO pk VALUES ('x')");
    dup(&mut e, "INSERT INTO pk VALUES ('X')");
    assert_eq!(count(&mut e, "SELECT COUNT(*) FROM pk"), 1);
}

/// An UPDATE that moves a row onto an existing folded key is rejected;
/// moving to a genuinely free value succeeds.
#[test]
fn update_onto_folded_key_is_rejected() {
    let mut e = mysql();
    ok(&mut e, "CREATE TABLE up (id INT, t VARCHAR(10) UNIQUE)");
    ok(&mut e, "INSERT INTO up VALUES (1,'a'),(2,'b')");
    dup(&mut e, "UPDATE up SET t='A' WHERE id=2"); // collides with 'a'
    ok(&mut e, "UPDATE up SET t='c' WHERE id=2"); // free value
    assert_eq!(count(&mut e, "SELECT COUNT(*) FROM up"), 2);
}

/// A byte-typed UNIQUE column keeps both byte-distinct values — it
/// stores bytes, not text, so folding never applies (MariaDB VARBINARY
/// UNIQUE keeps 'a' and 'A' both).
#[test]
fn varbinary_unique_keeps_both() {
    let mut e = mysql();
    ok(&mut e, "CREATE TABLE b (t VARBINARY(10) UNIQUE)");
    ok(&mut e, "INSERT INTO b VALUES ('a')");
    ok(&mut e, "INSERT INTO b VALUES ('A')");
    assert_eq!(count(&mut e, "SELECT COUNT(*) FROM b"), 2);
}

/// A PostgreSQL session is case-sensitive: both rows insert, the write
/// path does not fold. The dialect gate must not leak into PG.
#[test]
fn postgres_session_is_case_sensitive() {
    let mut p = Engine::new();
    ok(&mut p, "CREATE TABLE u (t TEXT UNIQUE)");
    ok(&mut p, "INSERT INTO u VALUES ('a')");
    ok(&mut p, "INSERT INTO u VALUES ('A')");
    ok(&mut p, "INSERT INTO u VALUES ('Bär')");
    ok(&mut p, "INSERT INTO u VALUES ('bar')");
    assert_eq!(count(&mut p, "SELECT COUNT(*) FROM u"), 4);
}
