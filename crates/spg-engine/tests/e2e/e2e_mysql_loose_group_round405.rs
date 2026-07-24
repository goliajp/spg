//! read01 round 405 (MySQL differential) — loose GROUP BY: a non-aggregated
//! column not in GROUP BY reads any (first-seen) row's value.
//!
//! MySQL allows `SELECT grp, name FROM g GROUP BY grp` even though `name` is
//! neither grouped nor aggregated — it returns the first-seen row's value
//! per group. PostgreSQL (and SPG until now) rejects it with "column name
//! does not exist" (its ONLY_FULL_GROUP_BY-equivalent). Under the MySQL
//! dialect a non-grouped column is wrapped in `any_value(col)`, so it rides
//! the existing aggregate machinery. A PostgreSQL session keeps the strict
//! rule.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn setup() -> Engine {
    let mut e = mysql();
    e.execute("CREATE TABLE g(grp INT, name VARCHAR(10), n INT)")
        .unwrap();
    e.execute("INSERT INTO g VALUES (1,'a',10),(1,'b',20),(2,'c',5),(2,'d',15)")
        .unwrap();
    e
}

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        Value::Int(n) => n.to_string(),
                        Value::BigInt(n) => n.to_string(),
                        Value::Text(s) => s.to_string(),
                        Value::Null => "NULL".to_string(),
                        o => panic!("{o:?}"),
                    })
                    .collect()
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// A non-grouped column returns the first-seen row's value.
#[test]
fn non_grouped_column_is_first_value() {
    let mut e = setup();
    assert_eq!(
        rows(&mut e, "SELECT grp, name, n FROM g GROUP BY grp ORDER BY grp"),
        vec![
            vec!["1", "a", "10"],
            vec!["2", "c", "5"],
        ]
    );
}

/// A non-grouped column rides alongside a real aggregate.
#[test]
fn with_aggregate() {
    let mut e = setup();
    assert_eq!(
        rows(&mut e, "SELECT grp, name, SUM(n) FROM g GROUP BY grp ORDER BY grp"),
        vec![
            vec!["1", "a", "30"],
            vec!["2", "c", "20"],
        ]
    );
}

/// A function over a non-grouped column wraps the column, not the call.
#[test]
fn function_over_non_grouped() {
    let mut e = setup();
    assert_eq!(
        rows(&mut e, "SELECT grp, UPPER(name) FROM g GROUP BY grp ORDER BY grp"),
        vec![vec!["1", "A"], vec!["2", "C"]]
    );
}

/// A fully-grouped / aggregated query is unchanged.
#[test]
fn strict_query_unchanged() {
    let mut e = setup();
    assert_eq!(
        rows(&mut e, "SELECT grp, COUNT(*) FROM g GROUP BY grp ORDER BY grp"),
        vec![vec!["1", "2"], vec!["2", "2"]]
    );
}

/// A PostgreSQL session keeps the strict rule.
#[test]
fn postgres_strict() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE g(grp INT, name VARCHAR(10))").unwrap();
    e.execute("INSERT INTO g VALUES (1,'a')").unwrap();
    assert!(
        e.execute("SELECT grp, name FROM g GROUP BY grp").is_err(),
        "PG rejects a non-grouped column"
    );
}
