//! read01 round 404 (MySQL differential) — HAVING can reference a
//! SELECT-list alias under the MySQL dialect.
//!
//! MySQL lets HAVING name a SELECT-list alias (`SELECT g, SUM(v) AS sv …
//! GROUP BY g HAVING sv > 30`); PostgreSQL requires the aggregate
//! expression itself and rejects the alias with "column sv does not
//! exist". SPG followed PG, so a common MySQL query failed. The alias is
//! substituted with its SELECT expression before the aggregate rewrite. A
//! PostgreSQL session keeps the strict rule.
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
    e.execute("CREATE TABLE s(g INT, v INT)").unwrap();
    e.execute("INSERT INTO s VALUES (1,10),(1,20),(2,5),(2,15),(2,25)")
        .unwrap();
    e
}

fn rows2(e: &mut Engine, sql: &str) -> Vec<(i64, i64)> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                let g = |v: &Value| match v {
                    Value::Int(n) => i64::from(*n),
                    Value::BigInt(n) => *n,
                    o => panic!("{o:?}"),
                };
                (g(&r.values[0]), g(&r.values[1]))
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// HAVING references the SUM alias.
#[test]
fn having_aggregate_alias() {
    let mut e = setup();
    assert_eq!(
        rows2(
            &mut e,
            "SELECT g, SUM(v) AS sv FROM s GROUP BY g HAVING sv > 30"
        ),
        vec![(2, 45)]
    );
}

/// The alias combines with a plain group-key predicate.
#[test]
fn alias_with_group_predicate() {
    let mut e = setup();
    assert_eq!(
        rows2(
            &mut e,
            "SELECT g, COUNT(*) AS c FROM s GROUP BY g HAVING c > 2 AND g > 0"
        ),
        vec![(2, 3)]
    );
}

/// HAVING with the aggregate spelled out (no alias) is unchanged.
#[test]
fn having_without_alias_unchanged() {
    let mut e = setup();
    let got: Vec<i64> = match e
        .execute("SELECT g FROM s GROUP BY g HAVING SUM(v) > 0 ORDER BY g")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                Value::Int(n) => i64::from(*n),
                o => panic!("{o:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    };
    assert_eq!(got, vec![1, 2]);
}

/// A PostgreSQL session keeps the strict rule (rejects the alias).
#[test]
fn postgres_rejects_alias() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE s(g INT, v INT)").unwrap();
    e.execute("INSERT INTO s VALUES (1,10),(2,25)").unwrap();
    assert!(
        e.execute("SELECT g, SUM(v) AS sv FROM s GROUP BY g HAVING sv > 30")
            .is_err(),
        "PG does not allow a HAVING alias"
    );
}
