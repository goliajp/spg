//! read01 round 413 (MySQL differential) — `UPDATE … [ORDER BY …] [LIMIT n]`.
//!
//! MySQL extends UPDATE with an optional ORDER BY + LIMIT so a bulk mutation
//! can pick the "first N rows in this order" — the common pattern for
//! draining a queue table, rate-limiting a migration, or picking the oldest
//! matching row. PostgreSQL has no such clause on UPDATE, so SPG's parser
//! rejected `UPDATE … ORDER BY …` outright. The MySQL dialect now accepts
//! and applies the clause; a PG session still errors on it.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn ints(e: &mut Engine, sql: &str) -> Vec<i64> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                Value::Int(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                o => panic!("{o:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn seed(e: &mut Engine, table: &str) {
    e.execute(&alloc::format!("CREATE TABLE {table}(n INT)")).unwrap();
    e.execute(&alloc::format!("INSERT INTO {table} VALUES(1),(2),(3),(4),(5)"))
        .unwrap();
}

/// ORDER BY ASC + LIMIT picks the smallest matched rows.
#[test]
fn asc_limit_picks_smallest() {
    let mut e = mysql();
    seed(&mut e, "ut");
    e.execute("UPDATE ut SET n = n + 100 ORDER BY n LIMIT 2").unwrap();
    // rows 1, 2 became 101, 102; 3, 4, 5 unchanged.
    assert_eq!(ints(&mut e, "SELECT n FROM ut ORDER BY n"), vec![3, 4, 5, 101, 102]);
}

/// ORDER BY DESC + LIMIT picks the largest matched rows.
#[test]
fn desc_limit_picks_largest() {
    let mut e = mysql();
    seed(&mut e, "ut");
    e.execute("UPDATE ut SET n = n + 100 ORDER BY n DESC LIMIT 2").unwrap();
    // rows 4, 5 became 104, 105; 1, 2, 3 unchanged.
    assert_eq!(ints(&mut e, "SELECT n FROM ut ORDER BY n"), vec![1, 2, 3, 104, 105]);
}

/// LIMIT without ORDER BY updates that many arbitrary rows (matched-count
/// bounded — verified via a count of the mutated ones).
#[test]
fn limit_alone_bounds_count() {
    let mut e = mysql();
    seed(&mut e, "ut");
    e.execute("UPDATE ut SET n = n + 100 LIMIT 2").unwrap();
    assert_eq!(ints(&mut e, "SELECT COUNT(*) FROM ut WHERE n > 100"), vec![2]);
    assert_eq!(ints(&mut e, "SELECT COUNT(*) FROM ut WHERE n <= 5"), vec![3]);
}

/// WHERE filters BEFORE ORDER BY / LIMIT (top-N of the filtered set).
#[test]
fn where_then_order_limit() {
    let mut e = mysql();
    seed(&mut e, "ut");
    e.execute("UPDATE ut SET n = n + 100 WHERE n > 2 ORDER BY n LIMIT 2")
        .unwrap();
    // WHERE keeps {3,4,5}; ORDER BY n LIMIT 2 picks 3, 4 -> 103, 104.
    assert_eq!(ints(&mut e, "SELECT n FROM ut ORDER BY n"), vec![1, 2, 5, 103, 104]);
}

/// A PostgreSQL session has no UPDATE ORDER BY clause and rejects the
/// statement at parse time.
#[test]
fn postgres_rejects() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ut(n INT)").unwrap();
    e.execute("INSERT INTO ut VALUES(1),(2),(3)").unwrap();
    assert!(
        e.execute("UPDATE ut SET n = n + 100 ORDER BY n LIMIT 1").is_err(),
        "PG has no UPDATE ORDER BY"
    );
}

extern crate alloc;
