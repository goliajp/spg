//! v7.39 (read01 round 145, parse_cte.c) — recursive-CTE well-formedness.
//! PG rejects several self-referencing shapes at parse analysis; SPG used to
//! silently COMPUTE three of them (INTERSECT / EXCEPT set-op form and a
//! self-reference on the nullable side of an outer join) and surfaced
//! misleading downstream errors for two more (aggregate in the recursive
//! term, self-ref inside an EXISTS sublink). Locked byte-identical against
//! PG 18.4 (15-case live matrix). Legal shapes (self-ref on the non-nullable
//! side of a LEFT JOIN, GROUP BY / DISTINCT in the recursive term,
//! non-self-referencing INTERSECT under WITH RECURSIVE) keep working.

use spg_engine::{Engine, QueryResult};

fn count(e: &mut Engine, sql: &str) -> i64 {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            spg_storage::Value::BigInt(n) => n,
            spg_storage::Value::Int(n) => i64::from(n),
            ref other => panic!("{other:?}"),
        },
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
    e.execute("CREATE TABLE ot(id int)").unwrap();
    e.execute("INSERT INTO ot VALUES (1)").unwrap();
}

#[test]
fn non_union_set_op_with_self_ref_rejected() {
    let mut e = Engine::new();
    // SPG used to compute 1 for both instead of erroring.
    errs(
        &mut e,
        "WITH RECURSIVE r(n) AS (SELECT 1 INTERSECT SELECT n FROM r) SELECT count(*) FROM r",
        "recursive query \"r\" does not have the form non-recursive-term UNION [ALL] recursive-term",
    );
    errs(
        &mut e,
        "WITH RECURSIVE r(n) AS (SELECT 1 EXCEPT SELECT n FROM r) SELECT count(*) FROM r",
        "does not have the form non-recursive-term UNION [ALL] recursive-term",
    );
    // Without a self-reference, any set-op shape is legal under WITH RECURSIVE
    // — and keeps its real set-op semantics. SPG's iterating materialiser used
    // to swallow these too, concatenating the arms like UNION ALL (INTERSECT
    // gave 2, EXCEPT gave 2, UNION-distinct gave 2).
    assert_eq!(
        count(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 INTERSECT SELECT 1) SELECT count(*) FROM r"
        ),
        1
    );
    assert_eq!(
        count(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 EXCEPT SELECT 1) SELECT count(*) FROM r"
        ),
        0
    );
    assert_eq!(
        count(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION SELECT 1) SELECT count(*) FROM r"
        ),
        1
    );
}

#[test]
fn outer_join_nullable_side_self_ref_rejected() {
    let mut e = Engine::new();
    setup(&mut e);
    // Self-ref on the NULLABLE side (right of LEFT JOIN) — SPG used to compute 2.
    errs(
        &mut e,
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT r.n+1 FROM ot LEFT JOIN r ON r.n = ot.id WHERE r.n < 3) SELECT count(*) FROM r",
        "recursive reference to query \"r\" must not appear within an outer join",
    );
    // Self-ref on the non-nullable side (left of LEFT JOIN) is legal — PG runs it.
    assert_eq!(
        count(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT r.n+1 FROM r LEFT JOIN ot ON r.n = ot.id WHERE r.n < 3) SELECT count(*) FROM r"
        ),
        3
    );
    // INNER JOIN with a self-ref is legal.
    assert_eq!(
        count(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT r.n+1 FROM r JOIN ot ON r.n = ot.id) SELECT count(*) FROM r"
        ),
        2
    );
}

#[test]
fn aggregate_in_recursive_term_rejected() {
    let mut e = Engine::new();
    // SPG used to run this and surface a misleading not-null violation.
    errs(
        &mut e,
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT max(n)+1 FROM r WHERE n<3) SELECT count(*) FROM r",
        "aggregate functions are not allowed in a recursive query's recursive term",
    );
    // GROUP BY / DISTINCT without aggregates stay legal (PG runs both).
    assert_eq!(
        count(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n<3 GROUP BY n) SELECT count(*) FROM r"
        ),
        3
    );
    assert_eq!(
        count(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT DISTINCT n+1 FROM r WHERE n<3) SELECT count(*) FROM r"
        ),
        3
    );
}

#[test]
fn sublink_and_anchor_self_refs_rejected() {
    let mut e = Engine::new();
    setup(&mut e);
    errs(
        &mut e,
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM ot WHERE EXISTS (SELECT 1 FROM r WHERE n < 3)) SELECT count(*) FROM r",
        "recursive reference to query \"r\" must not appear within a subquery",
    );
    errs(
        &mut e,
        "WITH RECURSIVE r(n) AS (SELECT n FROM r UNION ALL SELECT 1) SELECT count(*) FROM r",
        "recursive reference to query \"r\" must not appear within its non-recursive term",
    );
}
