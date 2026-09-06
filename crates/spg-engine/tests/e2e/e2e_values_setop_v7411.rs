//! v7.40.11 — `VALUES` could not be a branch of a set operation, which
//! is what breaks `\d`.
//!
//! Reported against 7.40.9 (§3.18):
//!
//! ```text
//!   SELECT 1 UNION ALL VALUES (2)    ERROR: syntax error at or near "VALUES"
//!   VALUES (1) UNION ALL SELECT 2    ERROR: syntax error at or near "UNION"
//!   SELECT 1 UNION ALL SELECT 2      1, 2
//! ```
//!
//! Modern psql builds its describe queries with `UNION ALL VALUES`, so
//! `\d`, `\dt`, `\di` and the rest fail against SPG with a syntax error
//! pointing into a query the user did not write. It is the first thing
//! anyone does with a PostgreSQL server.
//!
//! The parser already accepted VALUES in three of the four positions a
//! query block can occupy — as a statement, as a CTE body (where it
//! even ran the set-op chain, for `WITH RECURSIVE`), and inside
//! parentheses. The two that were missing are the two unparenthesised
//! ones: a bare VALUES as a set-op PEER, and a bare VALUES as the HEAD
//! of a chain.
//!
//! Every expectation below is measured on PostgreSQL 18.6.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn ints(eng: &mut Engine, sql: &str) -> Vec<i64> {
    match eng.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match r.values[0] {
                Value::Int(n) => i64::from(n),
                Value::BigInt(n) => n,
                Value::SmallInt(n) => i64::from(n),
                ref other => panic!("{sql}: {other:?}"),
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The two spellings that were filed.
#[test]
fn the_two_that_were_reported() {
    let mut eng = Engine::new();
    assert_eq!(ints(&mut eng, "SELECT 1 UNION ALL VALUES (2)"), vec![1, 2]);
    assert_eq!(ints(&mut eng, "VALUES (1) UNION ALL SELECT 2"), vec![1, 2]);
}

/// The rest of the class, because the parser has four query-block
/// positions and fixing the two that were filed would leave the
/// combinations.
///
/// ```text
///   VALUES (1),(2) UNION VALUES (2),(3)      1, 2, 3
///   SELECT 1 UNION ALL VALUES (2) ORDER BY   2, 1  (DESC)
///   SELECT 1 EXCEPT VALUES (1)               (no rows)
///   VALUES (1) INTERSECT SELECT 1            1
///   SELECT 1 UNION ALL VALUES (2) LIMIT 1    1
/// ```
#[test]
fn every_set_operation_takes_values_on_either_side() {
    let mut eng = Engine::new();
    let mut sorted = |sql: &str| {
        let mut v = ints(&mut eng, sql);
        v.sort_unstable();
        v
    };
    assert_eq!(
        sorted("VALUES (1),(2) UNION VALUES (2),(3)"),
        vec![1, 2, 3],
        "VALUES on both sides, duplicates folded"
    );
    assert_eq!(
        sorted("SELECT 1 EXCEPT VALUES (1)"),
        Vec::<i64>::new(),
        "EXCEPT"
    );
    assert_eq!(
        sorted("VALUES (1) INTERSECT SELECT 1"),
        vec![1],
        "INTERSECT"
    );
}

/// The tail binds to the whole chain, not to the VALUES branch.
#[test]
fn the_order_by_and_limit_belong_to_the_chain() {
    let mut eng = Engine::new();
    assert_eq!(
        ints(&mut eng, "SELECT 1 UNION ALL VALUES (2) ORDER BY 1 DESC"),
        vec![2, 1]
    );
    assert_eq!(
        ints(&mut eng, "SELECT 1 UNION ALL VALUES (2) LIMIT 1"),
        vec![1]
    );
    assert_eq!(
        ints(&mut eng, "VALUES (3) UNION ALL SELECT 1 ORDER BY 1"),
        vec![1, 3]
    );
}

/// The shape psql actually sends. `\d` builds its describe query this
/// way, which is why every backslash command failed.
#[test]
fn the_shape_psql_sends() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE vd (a INT)").unwrap();
    let got = ints(
        &mut eng,
        "SELECT count(*)::int FROM (SELECT c.relname FROM pg_class c WHERE c.relname = 'vd' \
         UNION ALL VALUES ('zzz')) s",
    );
    assert_eq!(got, vec![2]);
}

/// The positions that already worked, so a fix that reaches too far
/// cannot quietly change them.
#[test]
fn the_positions_that_already_worked_still_do() {
    let mut eng = Engine::new();
    assert_eq!(ints(&mut eng, "VALUES (1), (2)"), vec![1, 2], "statement");
    assert_eq!(
        ints(&mut eng, "WITH c(x) AS (VALUES (1),(2)) SELECT x FROM c"),
        vec![1, 2],
        "CTE body"
    );
    assert_eq!(
        ints(&mut eng, "(VALUES (1)) UNION ALL (VALUES (2))"),
        vec![1, 2],
        "parenthesized groups"
    );
    assert_eq!(
        ints(&mut eng, "SELECT x FROM (VALUES (1),(2)) v(x)"),
        vec![1, 2],
        "FROM position"
    );
    assert_eq!(
        ints(&mut eng, "SELECT 1 UNION ALL SELECT 2"),
        vec![1, 2],
        "the plain chain"
    );
}
