//! read01 round 411 (MySQL differential) — ORDER BY sorts by the session
//! collation.
//!
//! MariaDB's default collation `utf8mb4_uca1400_ai_ci` is case- and
//! accent-insensitive, so `ORDER BY v` puts `apple` before `Mango` before
//! `Zebra` and folds `Émile` next to `e`. SPG's ORDER BY built a byte-
//! lexicographic sort key (uppercase before lowercase), so a MySQL session
//! got rows back in ASCII order — a silent-wrong ordering. The precomputed
//! ORDER BY key now folds text under the MySQL dialect, matching the
//! fold-aware comparator (round 364). PostgreSQL keeps byte ordering.
//!
//! (MIN / MAX, GREATEST / LEAST, and CASE string comparison share this gap
//! and are addressed separately.)
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
        other => panic!("{other:?}"),
    }
}

fn setup() -> Engine {
    let mut e = mysql();
    e.execute("CREATE TABLE t(v VARCHAR(10))").unwrap();
    e.execute("INSERT INTO t VALUES('Zebra'),('apple'),('Mango'),('Émile')")
        .unwrap();
    e
}

/// ORDER BY folds case and accents (apple < Émile < Mango < Zebra).
#[test]
fn order_by_folds_ascending() {
    let mut e = setup();
    assert_eq!(
        col(&mut e, "SELECT v FROM t ORDER BY v"),
        vec!["apple", "Émile", "Mango", "Zebra"]
    );
}

/// DESC reverses the folded order.
#[test]
fn order_by_folds_descending() {
    let mut e = setup();
    assert_eq!(
        col(&mut e, "SELECT v FROM t ORDER BY v DESC"),
        vec!["Zebra", "Mango", "Émile", "apple"]
    );
}

/// A pure case difference sorts a non-colliding value correctly.
#[test]
fn order_by_case_fold() {
    let mut e = mysql();
    e.execute("CREATE TABLE c(v VARCHAR(10))").unwrap();
    e.execute("INSERT INTO c VALUES('banana'),('Apple'),('cherry')")
        .unwrap();
    assert_eq!(
        col(&mut e, "SELECT v FROM c ORDER BY v"),
        vec!["Apple", "banana", "cherry"]
    );
}

/// A PostgreSQL session keeps byte-lexicographic ordering (uppercase first).
#[test]
fn postgres_byte_order() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(v VARCHAR(10))").unwrap();
    e.execute("INSERT INTO t VALUES('Zebra'),('apple'),('Mango')")
        .unwrap();
    assert_eq!(
        col(&mut e, "SELECT v FROM t ORDER BY v"),
        vec!["Mango", "Zebra", "apple"]
    );
}
