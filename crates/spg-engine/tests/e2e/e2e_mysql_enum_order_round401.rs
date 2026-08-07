//! read01 round 401 (MySQL differential) — an inline `ENUM(...)` column
//! sorts by its variant declaration order, not alphabetically.
//!
//! MySQL sorts an ENUM column by the ordinal of each value in the declared
//! variant list: `ENUM('low','mid','high')` orders low, mid, high — not the
//! alphabetical high, low, mid. SPG stored an inline ENUM as text and sorted
//! it as text, so `ORDER BY e` returned the rows in the wrong order, a
//! silent-wrong. This mirrors what SPG already does for a PG CREATE TYPE
//! enum (enumsortorder).
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                Value::Text(s) => s.to_string(),
                o => panic!("{o:?}"),
            })
            .collect(),
        other => panic!("`{sql}`: {other:?}"),
    }
}

fn setup() -> Engine {
    let mut e = mysql();
    e.execute("CREATE TABLE en(e ENUM('low','mid','high'))")
        .unwrap();
    e.execute("INSERT INTO en VALUES ('high'),('low'),('mid')")
        .unwrap();
    e
}

/// ORDER BY sorts by declaration order, ascending.
#[test]
fn order_by_ordinal_asc() {
    let mut e = setup();
    assert_eq!(
        col(&mut e, "SELECT e FROM en ORDER BY e"),
        vec!["low", "mid", "high"]
    );
}

/// ORDER BY DESC is the reverse ordinal order.
#[test]
fn order_by_ordinal_desc() {
    let mut e = setup();
    assert_eq!(
        col(&mut e, "SELECT e FROM en ORDER BY e DESC"),
        vec!["high", "mid", "low"]
    );
}

/// A larger set keeps the declared order (not alphabetical).
#[test]
fn declaration_order_not_alphabetical() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(s ENUM('banana','apple','cherry'))")
        .unwrap();
    e.execute("INSERT INTO t VALUES ('cherry'),('apple'),('banana')")
        .unwrap();
    // declared order banana, apple, cherry — NOT alphabetical
    assert_eq!(
        col(&mut e, "SELECT s FROM t ORDER BY s"),
        vec!["banana", "apple", "cherry"]
    );
}
