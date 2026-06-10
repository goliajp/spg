//! v7.17.0 Phase 3.P0-46 — MySQL TINYINT(1) INSERT-time int → bool coerce.
//!
//! Phase 4.3 already classified `TINYINT(1)` as `DataType::Bool` at
//! CREATE TABLE time, but the INSERT path required a `TRUE` / `FALSE`
//! literal. mysqldump emits the values as integer `0` / `1`, so the
//! 0-change cutover broke on every dump-restore.
//!
//! P0-46 adds the int → bool (and "any non-zero is truthy", MySQL
//! semantics) coerce arms inside `coerce_value`, so all int widths
//! land cleanly into a TINYINT(1) column.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn insert_int_one_becomes_bool_true() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, flag TINYINT(1) NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    let r = rows(e.execute("SELECT flag FROM t").unwrap());
    assert_eq!(r[0][0], Value::Bool(true));
}

#[test]
fn insert_int_zero_becomes_bool_false() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, flag TINYINT(1) NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 0)").unwrap();
    let r = rows(e.execute("SELECT flag FROM t").unwrap());
    assert_eq!(r[0][0], Value::Bool(false));
}

#[test]
fn insert_mixed_int_literals_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, flag TINYINT(1) NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 0), (2, 1), (3, 1)")
        .unwrap();
    let r = rows(e.execute("SELECT flag FROM t ORDER BY id").unwrap());
    assert_eq!(r[0][0], Value::Bool(false));
    assert_eq!(r[1][0], Value::Bool(true));
    assert_eq!(r[2][0], Value::Bool(true));
}

#[test]
fn nonzero_int_is_truthy_mysql_semantics() {
    // MySQL: any non-zero integer is truthy when stored in TINYINT(1).
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, flag TINYINT(1) NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 2), (2, 99), (3, -1)")
        .unwrap();
    let r = rows(e.execute("SELECT flag FROM t ORDER BY id").unwrap());
    assert_eq!(r[0][0], Value::Bool(true));
    assert_eq!(r[1][0], Value::Bool(true));
    assert_eq!(r[2][0], Value::Bool(true));
}

#[test]
fn null_remains_null_for_nullable_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, flag TINYINT(1) NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, NULL)").unwrap();
    let r = rows(e.execute("SELECT flag FROM t").unwrap());
    assert_eq!(r[0][0], Value::Null);
}

#[test]
fn update_int_to_tinyint1_coerces() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, flag TINYINT(1) NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 0)").unwrap();
    e.execute("UPDATE t SET flag = 1 WHERE id = 1").unwrap();
    let r = rows(e.execute("SELECT flag FROM t").unwrap());
    assert_eq!(r[0][0], Value::Bool(true));
}

#[test]
fn bigint_literal_coerces() {
    // Wide MySQL dumps sometimes emit the boolean column with a
    // BIGINT-shaped literal (e.g. `1000000`). Still truthy.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, flag TINYINT(1) NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 1000000)").unwrap();
    let r = rows(e.execute("SELECT flag FROM t").unwrap());
    assert_eq!(r[0][0], Value::Bool(true));
}

#[test]
fn boolean_literal_still_works_phase_4_3_regression() {
    // Phase 4.3 already supported `TRUE` / `FALSE` literals;
    // P0-46 must not break that path.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, flag TINYINT(1) NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, TRUE), (2, FALSE)")
        .unwrap();
    let r = rows(e.execute("SELECT flag FROM t ORDER BY id").unwrap());
    assert_eq!(r[0][0], Value::Bool(true));
    assert_eq!(r[1][0], Value::Bool(false));
}

#[test]
fn explicit_bool_column_also_accepts_int_literals() {
    // PG-canonical `BOOLEAN` (which SPG also exposes via the
    // `bool` keyword) must accept `0` / `1` integer INSERT too —
    // PG itself does not, but the customer surface (PG-clones
    // like ClickHouse, MySQL) does, and we want a single coerce
    // path for both spellings.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, flag BOOLEAN NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 0), (2, 1)").unwrap();
    let r = rows(e.execute("SELECT flag FROM t ORDER BY id").unwrap());
    assert_eq!(r[0][0], Value::Bool(false));
    assert_eq!(r[1][0], Value::Bool(true));
}
