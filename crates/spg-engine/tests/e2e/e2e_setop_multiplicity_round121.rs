//! v7.39 (read01 round 121, Track A — nodeSetOp.c 补读) — INTERSECT / EXCEPT
//! multiplicity semantics, locked byte-identical against PG 18.4.
//!
//! Read-driven scan of `src/backend/executor/nodeSetOp.c`: no SPG divergence.
//! These pins lock the per-group output-count rules against regression —
//! INTERSECT ALL emits min(left, right) copies, EXCEPT ALL emits
//! max(0, left − right), the distinct variants emit 0 or 1, and NULL groups
//! with NULL (set ops use not-distinct equality).

use spg_engine::{Engine, QueryResult};

fn count(e: &mut Engine, sql: &str) -> i64 {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::BigInt(n) => *n,
            spg_storage::Value::Int(n) => i64::from(*n),
            other => panic!("{sql}: {other:?}"),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn intersect_all_is_min() {
    let mut e = Engine::new();
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM ((VALUES('a'),('a'),('a')) INTERSECT ALL (VALUES('a'),('a'))) s"
        ),
        2
    );
    // Multi-group: 2→min(2,3)=2, 3→min(1,1)=1.
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM ((VALUES(1),(2),(2),(3)) INTERSECT ALL (VALUES(2),(2),(2),(3))) s"
        ),
        3
    );
}

#[test]
fn except_all_is_saturating_subtract() {
    let mut e = Engine::new();
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM ((VALUES('a'),('a'),('a')) EXCEPT ALL (VALUES('a'))) s"
        ),
        2
    );
    // left < right → 0 (no negative counts).
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM ((VALUES('a')) EXCEPT ALL (VALUES('a'),('a'),('a'))) s"
        ),
        0
    );
    // Multi-group: 1→2−1=1, 2→1−0=1.
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM ((VALUES(1),(1),(2)) EXCEPT ALL (VALUES(1),(3))) s"
        ),
        2
    );
}

#[test]
fn distinct_variants_emit_zero_or_one() {
    let mut e = Engine::new();
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM ((VALUES('a'),('a'),('a')) INTERSECT (VALUES('a'),('a'))) s"
        ),
        1
    );
    // Distinct EXCEPT removes the group entirely if the right has any match.
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM ((VALUES('a'),('a')) EXCEPT (VALUES('a'))) s"
        ),
        0
    );
}

#[test]
fn null_groups_with_null() {
    let mut e = Engine::new();
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM ((VALUES(NULL::int)) INTERSECT (VALUES(NULL::int))) s"
        ),
        1
    );
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM ((VALUES(NULL::int),(1)) EXCEPT ALL (VALUES(1))) s"
        ),
        1
    );
}
