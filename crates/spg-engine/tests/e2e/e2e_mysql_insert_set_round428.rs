//! read01 round 428 (MySQL differential) — the SET-form INSERT.
//!
//! `INSERT INTO t SET a = 1, b = 'x'` is MySQL's assignment-style spelling
//! of an INSERT, common in older MySQL code and some ORMs. SPG had no such
//! grammar — every one was `syntax error at or near "SET"`.
//!
//! Measured on MariaDB 11, it is EXACTLY
//! `INSERT INTO t (a, b) VALUES (1, 'x')`: omitted columns take their
//! DEFAULT, `SET a = DEFAULT` is legal, values may be arbitrary
//! expressions, AUTO_INCREMENT / LAST_INSERT_ID behave as usual, and it
//! composes with `IGNORE`, `ON DUPLICATE KEY UPDATE`, `REPLACE`, and
//! `RETURNING`. So it lowers to the column list plus one VALUES row and
//! rejoins the ordinary path, which already handles all of those.
//!
//! PostgreSQL has no such spelling and still rejects it.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn row(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(|v| match v {
                Value::Null => "NULL".to_string(),
                other => spg_engine::eval::value_to_text(other),
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn row_count(e: &mut Engine) -> i64 {
    match e.execute("SELECT ROW_COUNT()").unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::BigInt(n) => *n,
            Value::Int(n) => i64::from(*n),
            o => panic!("{o:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = mysql();
    e.execute(
        "CREATE TABLE t(id INT PRIMARY KEY AUTO_INCREMENT, \
         a INT DEFAULT 7, b VARCHAR(10) DEFAULT 'dd')",
    )
    .unwrap();
    e
}

/// The basic form, and an omitted column taking its declared default.
#[test]
fn basic_and_defaults() {
    let mut e = seeded();
    e.execute("INSERT INTO t SET a = 1, b = 'x'").unwrap();
    assert_eq!(row(&mut e, "SELECT id, a, b FROM t"), vec!["1", "1", "x"]);
    // `a` omitted -> DEFAULT 7; id auto-increments.
    e.execute("INSERT INTO t SET b = 'y'").unwrap();
    assert_eq!(
        row(&mut e, "SELECT id, a, b FROM t WHERE b = 'y'"),
        vec!["2", "7", "y"]
    );
}

/// An explicit `DEFAULT` on the right-hand side.
#[test]
fn explicit_default_keyword() {
    let mut e = seeded();
    e.execute("INSERT INTO t SET a = DEFAULT, b = 'z'").unwrap();
    assert_eq!(
        row(&mut e, "SELECT a, b FROM t WHERE b = 'z'"),
        vec!["7", "z"]
    );
}

/// Values may be arbitrary expressions.
#[test]
fn expression_values() {
    let mut e = seeded();
    e.execute("INSERT INTO t SET a = 2 + 3, b = CONCAT('p','q')")
        .unwrap();
    assert_eq!(
        row(&mut e, "SELECT a, b FROM t WHERE b = 'pq'"),
        vec!["5", "pq"]
    );
}

/// It composes with IGNORE / ON DUPLICATE KEY UPDATE / REPLACE.
#[test]
fn composes_with_upsert_forms() {
    let mut e = seeded();
    e.execute("INSERT INTO t SET id = 1, a = 1, b = 'x'")
        .unwrap();
    // ON DUPLICATE KEY UPDATE
    e.execute("INSERT INTO t SET id = 1, a = 99 ON DUPLICATE KEY UPDATE a = 99")
        .unwrap();
    assert_eq!(row(&mut e, "SELECT a FROM t WHERE id = 1"), vec!["99"]);
    // IGNORE skips the conflicting row.
    e.execute("INSERT IGNORE INTO t SET id = 1, a = 1000")
        .unwrap();
    assert_eq!(row(&mut e, "SELECT a FROM t WHERE id = 1"), vec!["99"]);
    // REPLACE takes the incoming row.
    e.execute("REPLACE INTO t SET id = 1, a = 5, b = 'r'")
        .unwrap();
    assert_eq!(
        row(&mut e, "SELECT a, b FROM t WHERE id = 1"),
        vec!["5", "r"]
    );
}

/// AUTO_INCREMENT / LAST_INSERT_ID behave as they do for the VALUES form.
#[test]
fn last_insert_id_tracks() {
    let mut e = seeded();
    e.execute("INSERT INTO t SET a = 1, b = 'x'").unwrap();
    e.execute("INSERT INTO t SET a = 2, b = 'y'").unwrap();
    assert_eq!(row(&mut e, "SELECT LAST_INSERT_ID()"), vec!["2"]);
}

/// ROW_COUNT() sees it as the insert / upsert it is (rounds 426-427).
#[test]
fn row_count_and_returning() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(id INT PRIMARY KEY, a INT)")
        .unwrap();
    e.execute("INSERT INTO t SET id = 1, a = 10").unwrap();
    assert_eq!(row_count(&mut e), 1);
    // Conflict that changes nothing -> 0.
    e.execute("INSERT INTO t SET id = 1, a = 10 ON DUPLICATE KEY UPDATE a = 10")
        .unwrap();
    assert_eq!(row_count(&mut e), 0);
    // Conflict that changes the row -> 2.
    e.execute("INSERT INTO t SET id = 1, a = 11 ON DUPLICATE KEY UPDATE a = 11")
        .unwrap();
    assert_eq!(row_count(&mut e), 2);
    // RETURNING works on the SET form too.
    e.execute("CREATE TABLE s(v INT)").unwrap();
    assert_eq!(
        row(&mut e, "INSERT INTO s SET v = 5 RETURNING v"),
        vec!["5"]
    );
}

/// A PostgreSQL session has no SET-form INSERT.
#[test]
fn postgres_rejects() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id INT PRIMARY KEY, a INT)")
        .unwrap();
    assert!(
        e.execute("INSERT INTO t SET id = 1, a = 10").is_err(),
        "PG has no SET-form INSERT"
    );
}
