//! read01 round 387 (MySQL type-fidelity epic — P2) — a TINYINT /
//! MEDIUMINT column enforces its declared range on INSERT / UPDATE.
//!
//! The silent-wrong P1 set up: `INSERT 128 INTO TINYINT` was stored
//! silently (SmallInt holds 128) where MariaDB strict raises ERROR 1264
//! "Out of range value for column 'a'". P2 checks the value against the
//! declared width's bounds at the write path:
//!   TINYINT           -128 .. 127        TINYINT UNSIGNED   0 .. 255
//!   MEDIUMINT   -8388608 .. 8388607      MEDIUMINT UNSIGNED 0 .. 16777215
//! A boundary value still inserts; SMALLINT / INT are unaffected (their
//! storage type already enforces the range); a PostgreSQL session keeps
//! its lenient TINYINT-as-SmallInt behavior.
//!
//! Every boundary / rejection is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn ok(e: &mut Engine, sql: &str) {
    assert!(
        matches!(e.execute(sql), Ok(QueryResult::CommandOk { .. })),
        "expected `{sql}` to succeed"
    );
}

fn out_of_range(e: &mut Engine, sql: &str) {
    match e.execute(sql) {
        Err(err) => assert!(
            err.to_string().contains("Out of range value for column"),
            "`{sql}` should be an out-of-range error, got: {err}"
        ),
        other => panic!("`{sql}` should have errored, got {other:?}"),
    }
}

/// The boundary values all insert.
#[test]
fn boundaries_insert() {
    let mut e = mysql();
    e.execute("CREATE TABLE r(a TINYINT, b TINYINT UNSIGNED, c MEDIUMINT, d MEDIUMINT UNSIGNED)")
        .unwrap();
    ok(&mut e, "INSERT INTO r(a) VALUES(127)");
    ok(&mut e, "INSERT INTO r(a) VALUES(-128)");
    ok(&mut e, "INSERT INTO r(b) VALUES(255)");
    ok(&mut e, "INSERT INTO r(b) VALUES(0)");
    ok(&mut e, "INSERT INTO r(c) VALUES(8388607)");
    ok(&mut e, "INSERT INTO r(c) VALUES(-8388608)");
    ok(&mut e, "INSERT INTO r(d) VALUES(16777215)");
}

/// A value past the declared bound is rejected on INSERT.
#[test]
fn overflow_rejected() {
    let mut e = mysql();
    e.execute("CREATE TABLE r(a TINYINT, b TINYINT UNSIGNED, c MEDIUMINT, d MEDIUMINT UNSIGNED)")
        .unwrap();
    out_of_range(&mut e, "INSERT INTO r(a) VALUES(128)");
    out_of_range(&mut e, "INSERT INTO r(a) VALUES(-129)");
    out_of_range(&mut e, "INSERT INTO r(b) VALUES(256)");
    out_of_range(&mut e, "INSERT INTO r(b) VALUES(-1)");
    out_of_range(&mut e, "INSERT INTO r(c) VALUES(8388608)");
    out_of_range(&mut e, "INSERT INTO r(c) VALUES(-8388609)");
    out_of_range(&mut e, "INSERT INTO r(d) VALUES(16777216)");
}

/// UPDATE enforces the range too.
#[test]
fn update_enforces_range() {
    let mut e = mysql();
    e.execute("CREATE TABLE r(a TINYINT)").unwrap();
    ok(&mut e, "INSERT INTO r VALUES(10)");
    out_of_range(&mut e, "UPDATE r SET a = 200");
    // an in-range UPDATE still applies
    ok(&mut e, "UPDATE r SET a = 100");
}

/// SMALLINT / INT are unaffected (their storage type already bounds them).
#[test]
fn wider_types_unaffected() {
    let mut e = mysql();
    e.execute("CREATE TABLE r(s SMALLINT, i INT)").unwrap();
    ok(&mut e, "INSERT INTO r(s) VALUES(30000)");
    ok(&mut e, "INSERT INTO r(i) VALUES(2000000000)");
}

/// A PostgreSQL session keeps the lenient TINYINT-as-SmallInt behavior.
#[test]
fn postgres_unaffected() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(a TINYINT)").unwrap();
    ok(&mut e, "INSERT INTO t VALUES(128)");
}
