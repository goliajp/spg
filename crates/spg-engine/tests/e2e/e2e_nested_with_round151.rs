//! v7.39 (read01 round 151) — nested WITH, per parse_cte.c / view.c.
//! Read-only nested WITH is legal PG everywhere a subquery goes; SPG used
//! to reject FIVE such positions at parse (CTE body head, EXISTS, IN,
//! INSERT source both parenthesized and bare, view / matview bodies).
//! A data-modifying CTE below the top level errors `WITH clause containing
//! a data-modifying statement must be at the top level` (0A000) with NO
//! side effect; view bodies get their own wording (`views must not
//! contain…` / `materialized views must not use…`). Locked byte-identical
//! against PG 18.4 (19-case live matrix, r151 probes).

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> i64 {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            spg_storage::Value::BigInt(n) => n,
            spg_storage::Value::Int(n) => i64::from(n),
            ref other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn affected(e: &mut Engine, sql: &str) -> usize {
    match e.execute(sql).unwrap() {
        QueryResult::CommandOk { affected, .. } => affected,
        other => panic!("{other:?}"),
    }
}

fn errs(e: &mut Engine, sql: &str, want: &str) {
    let m = match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(r) => panic!("expected error for {sql}, got {r:?}"),
    };
    assert!(m.contains(want), "{m}");
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE t151(id int)").unwrap();
    e.execute("INSERT INTO t151 VALUES (1),(2)").unwrap();
}

const TOP: &str = "WITH clause containing a data-modifying statement must be at the top level";

/// Read-only nested WITH works in every subquery position.
#[test]
fn readonly_nested_with_accepted_everywhere() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        one(
            &mut e,
            "SELECT * FROM (WITH c AS (SELECT 1 AS id) SELECT * FROM c) x",
        ),
        1
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT (WITH c AS (SELECT 1 AS id) SELECT count(*) FROM c)",
        ),
        1
    );
    assert_eq!(
        one(
            &mut e,
            "WITH x AS (WITH c AS (SELECT 1 AS id) SELECT * FROM c) SELECT * FROM x",
        ),
        1
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT 1 WHERE EXISTS (WITH c AS (SELECT 1 AS id) SELECT * FROM c)",
        ),
        1
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT * FROM t151 WHERE id IN (WITH c AS (SELECT 1 AS id) SELECT id FROM c)",
        ),
        1
    );
}

/// INSERT sources: parenthesized query, bare WITH-headed query, and a
/// WITH-headed INSERT as a CTE body (PreparableStmt carries its own WITH).
#[test]
fn readonly_nested_with_insert_sources() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO t151 (WITH c AS (SELECT 8 AS id) SELECT id FROM c)",
        ),
        1
    );
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO t151 WITH c AS (SELECT 7 AS id) SELECT id FROM c",
        ),
        1
    );
    assert_eq!(
        one(
            &mut e,
            "WITH i AS (WITH c AS (SELECT 5 AS id) \
                        INSERT INTO t151 SELECT id FROM c RETURNING id) \
             SELECT * FROM i",
        ),
        5
    );
    assert_eq!(one(&mut e, "SELECT count(*) FROM t151"), 5);
}

/// View and matview bodies accept read-only WITH; reads through the view
/// resolve the CTE.
#[test]
fn readonly_with_in_view_bodies() {
    let mut e = Engine::new();
    setup(&mut e);
    affected(
        &mut e,
        "CREATE VIEW v151 AS WITH c AS (SELECT 1 AS id) SELECT * FROM c",
    );
    assert_eq!(one(&mut e, "SELECT * FROM v151"), 1);
    affected(
        &mut e,
        "CREATE MATERIALIZED VIEW mv151 AS WITH c AS (SELECT 2 AS id) SELECT * FROM c",
    );
    assert_eq!(one(&mut e, "SELECT * FROM mv151"), 2);
}

/// A data-modifying CTE below the top level errors with PG's message and
/// runs nothing — the table stays intact after every rejection.
#[test]
fn modifying_nested_with_rejected_without_side_effect() {
    let mut e = Engine::new();
    setup(&mut e);
    errs(
        &mut e,
        "SELECT * FROM (WITH d AS (DELETE FROM t151 RETURNING id) SELECT * FROM d) x",
        TOP,
    );
    errs(
        &mut e,
        "SELECT (WITH d AS (DELETE FROM t151 RETURNING id) SELECT count(*) FROM d)",
        TOP,
    );
    errs(
        &mut e,
        "WITH x AS (WITH d AS (DELETE FROM t151 RETURNING id) SELECT * FROM d) SELECT * FROM x",
        TOP,
    );
    errs(
        &mut e,
        "SELECT 1 WHERE EXISTS (WITH d AS (DELETE FROM t151 RETURNING id) SELECT * FROM d)",
        TOP,
    );
    errs(
        &mut e,
        "SELECT * FROM t151 WHERE id IN (WITH d AS (DELETE FROM t151 RETURNING id) SELECT id FROM d)",
        TOP,
    );
    errs(
        &mut e,
        "INSERT INTO t151 (WITH d AS (DELETE FROM t151 RETURNING id) SELECT id+10 FROM d)",
        TOP,
    );
    errs(
        &mut e,
        "INSERT INTO t151 WITH d AS (DELETE FROM t151 RETURNING id) SELECT id+10 FROM d",
        TOP,
    );
    assert_eq!(one(&mut e, "SELECT count(*) FROM t151"), 2);
}

/// View bodies use PG's view-specific wording.
#[test]
fn modifying_with_in_view_bodies_rejected() {
    let mut e = Engine::new();
    setup(&mut e);
    errs(
        &mut e,
        "CREATE VIEW v151m AS WITH d AS (DELETE FROM t151 RETURNING id) SELECT * FROM d",
        "views must not contain data-modifying statements in WITH",
    );
    errs(
        &mut e,
        "CREATE MATERIALIZED VIEW mv151m AS WITH d AS (DELETE FROM t151 RETURNING id) SELECT * FROM d",
        "materialized views must not use data-modifying statements in WITH",
    );
    assert_eq!(one(&mut e, "SELECT count(*) FROM t151"), 2);
}
