//! v7.39 (read01 round 186, A-recwf) — a parenthesized recursive
//! UNION peer (PG's legal way to put ORDER BY / LIMIT on the
//! recursive term) actually recurses.
//!
//! Live-PG18 differential 2026-07-18:
//!   WITH RECURSIVE t(n) AS
//!     (SELECT 1 UNION ALL (SELECT n+1 FROM t WHERE n < 3
//!                          ORDER BY n LIMIT 1))
//!   SELECT * FROM t  →  1,2,3
//! SPG parsed the parenthesized peer as `SELECT * FROM (inner) sub`,
//! the name-only recursion check missed the inner `t`, the peer was
//! treated as a non-recursive anchor term and failed with
//! `relation "t" does not exist`.

use spg_engine::{Engine, QueryResult};

fn ints(e: &mut Engine, sql: &str) -> Vec<i64> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match r.values[0] {
                spg_storage::Value::Int(n) => i64::from(n),
                spg_storage::Value::BigInt(n) => n,
                ref other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn paren_recursive_peer_recurses() {
    let mut e = Engine::new();
    assert_eq!(
        ints(
            &mut e,
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL \
             (SELECT n+1 FROM t WHERE n < 3 ORDER BY n LIMIT 1)) \
             SELECT * FROM t"
        ),
        [1, 2, 3]
    );
}

#[test]
fn unparenthesized_order_by_still_rejected() {
    // PG: "ORDER BY in a recursive query is not implemented" — the
    // r186 fix must not accidentally legalise the unparenthesized
    // form (already aligned pre-r186).
    let mut e = Engine::new();
    let err = e
        .execute(
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL \
             SELECT n+1 FROM t WHERE n < 3 ORDER BY n LIMIT 1) \
             SELECT * FROM t",
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("ORDER BY in a recursive query is not implemented"),
        "unexpected: {err}"
    );
}

#[test]
fn nonrecursive_paren_peer_unchanged() {
    let mut e = Engine::new();
    assert_eq!(
        ints(
            &mut e,
            "WITH t AS (SELECT 5 AS n UNION ALL (SELECT 6 ORDER BY 1)) SELECT * FROM t"
        ),
        [5, 6]
    );
}

#[test]
fn derived_alias_shadowing_not_overmatched() {
    // A derived table aliased with the CTE's own name does NOT make
    // the peer recursive: the alias shadows the outer name. The peer
    // here references only the physical table s186, so it is an
    // anchor term and the query completes without iteration.
    let mut e = Engine::new();
    e.execute("CREATE TABLE s186 (n INT)").unwrap();
    e.execute("INSERT INTO s186 VALUES (7)").unwrap();
    assert_eq!(
        ints(
            &mut e,
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL \
             (SELECT * FROM (SELECT n FROM s186) t)) SELECT * FROM t ORDER BY n"
        ),
        [1, 7]
    );
}
