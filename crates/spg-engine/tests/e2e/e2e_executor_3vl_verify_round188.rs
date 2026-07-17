//! v7.39 (read01 round 188) — Track-2 executor verify-set (B07/B08),
//! all verified CORRECT against live PG18 (2026-07-18, 7/7 SAME) and
//! pinned here so a future executor change can't regress them:
//!   * NOT IN with a NULL in the list → empty (3VL: nothing passes);
//!   * NOT IN with NULL probe rows → NULL row filtered, rest pass;
//!   * inner hash join never matches NULL = NULL;
//!   * LEFT JOIN NULL-key rows survive unmatched;
//!   * recursive UNION (distinct) dedup terminates a cyclic feed;
//!   * correlated subquery re-evaluates per outer row (no memo
//!     cross-contamination).

use spg_engine::{Engine, QueryResult};

fn cell_strings(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| format!("{v:?}"))
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn not_in_with_null_in_list_is_empty() {
    let mut e = Engine::new();
    assert!(cell_strings(
        &mut e,
        "SELECT x FROM (VALUES (1),(2),(3)) v(x) \
         WHERE x NOT IN (SELECT y FROM (VALUES (2),(NULL::int)) w(y))"
    )
    .is_empty());
    // Longer list, same 3VL rule.
    assert!(cell_strings(
        &mut e,
        "SELECT x FROM (VALUES (1),(2),(3),(4),(5),(6),(7),(8)) v(x) \
         WHERE x NOT IN (SELECT y FROM (VALUES (1),(3),(5),(7),(NULL::int)) w(y))"
    )
    .is_empty());
}

#[test]
fn not_in_null_probe_row_filtered() {
    let mut e = Engine::new();
    assert_eq!(
        cell_strings(
            &mut e,
            "SELECT x FROM (VALUES (1),(2),(NULL::int)) v(x) \
             WHERE x NOT IN (SELECT y FROM (VALUES (5),(6)) w(y)) ORDER BY x"
        )
        .len(),
        2
    );
}

#[test]
fn join_null_keys_never_match() {
    let mut e = Engine::new();
    assert_eq!(
        cell_strings(
            &mut e,
            "SELECT count(*) FROM (VALUES (1),(NULL::int)) a(x) \
             JOIN (VALUES (1),(NULL::int)) b(y) ON a.x = b.y"
        ),
        ["BigInt(1)"]
    );
    assert_eq!(
        cell_strings(
            &mut e,
            "SELECT count(*) FROM (VALUES (1),(NULL::int)) a(x) \
             LEFT JOIN (VALUES (1),(NULL::int)) b(y) ON a.x = b.y"
        ),
        ["BigInt(2)"]
    );
}

#[test]
fn recursive_union_distinct_dedup_terminates() {
    let mut e = Engine::new();
    assert_eq!(
        cell_strings(
            &mut e,
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION SELECT (n % 3) + 1 FROM t) \
             SELECT * FROM t ORDER BY n"
        )
        .len(),
        3
    );
}

#[test]
fn correlated_subquery_no_cross_contamination() {
    let mut e = Engine::new();
    // NOTE r188: the VALUE semantics are pinned; the count's TYPE in a
    // correlated scalar subquery is `integer` here vs PG's `bigint` —
    // recorded in the task-design as a separate open item, do not pin
    // the wrong type.
    let rows = cell_strings(
        &mut e,
        "SELECT a.x, (SELECT count(*) FROM (VALUES (1),(2)) b(y) WHERE b.y <= a.x) \
         FROM (VALUES (1),(2)) a(x) ORDER BY a.x",
    );
    assert_eq!(rows.len(), 2);
    assert!(rows[0].ends_with("(1)") && rows[0].starts_with("Int(1)|"), "{rows:?}");
    assert!(rows[1].ends_with("(2)") && rows[1].starts_with("Int(2)|"), "{rows:?}");
}
