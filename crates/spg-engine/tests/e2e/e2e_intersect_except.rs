//! v7.37.17 (17.6 siblings) — INTERSECT [ALL] + EXCEPT [ALL] set
//! operations.

use spg_engine::{Engine, QueryResult};

fn ints(e: &mut Engine, sql: &str) -> Vec<i64> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter()
        .map(|row| match &row.values[0] {
            spg_storage::Value::Int(n) => i64::from(*n),
            spg_storage::Value::BigInt(n) => *n,
            other => panic!("expected integer, got {other:?}"),
        })
        .collect()
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE l (x INT)").unwrap();
    // Left multiset: {1, 2, 2, 3}.
    e.execute("INSERT INTO l VALUES (1), (2), (2), (3)")
        .unwrap();
    e.execute("CREATE TABLE r (x INT)").unwrap();
    // Right multiset: {2, 3, 3, 4}.
    e.execute("INSERT INTO r VALUES (2), (3), (3), (4)")
        .unwrap();
}

#[test]
fn intersect_distinct_and_all() {
    let mut e = Engine::new();
    setup(&mut e);
    // INTERSECT: distinct rows on both sides → {2, 3}.
    assert_eq!(
        ints(
            &mut e,
            "SELECT x FROM l INTERSECT SELECT x FROM r ORDER BY x"
        ),
        [2, 3]
    );
    // INTERSECT ALL: min counts — 2 appears min(2,1)=1×, 3 min(1,2)=1×.
    assert_eq!(
        ints(
            &mut e,
            "SELECT x FROM l INTERSECT ALL SELECT x FROM r ORDER BY x"
        ),
        [2, 3]
    );
}

#[test]
fn except_distinct_and_all() {
    let mut e = Engine::new();
    setup(&mut e);
    // EXCEPT: distinct left rows absent from the right → {1}.
    assert_eq!(
        ints(&mut e, "SELECT x FROM l EXCEPT SELECT x FROM r ORDER BY x"),
        [1]
    );
    // EXCEPT ALL: {1,2,2,3} - {2,3,3,4} → {1, 2}.
    assert_eq!(
        ints(
            &mut e,
            "SELECT x FROM l EXCEPT ALL SELECT x FROM r ORDER BY x"
        ),
        [1, 2]
    );
}

#[test]
fn intersect_binds_tighter_than_union() {
    let mut e = Engine::new();
    setup(&mut e);
    // PG precedence: A UNION B INTERSECT C = A ∪ (B ∩ C).
    // Here: {1} ∪ ({1,2,2,3} ∩ {2,3,3,4}) = {1} ∪ {2,3} = {1,2,3}.
    assert_eq!(
        ints(
            &mut e,
            "SELECT 1 UNION SELECT x FROM l INTERSECT SELECT x FROM r \
             ORDER BY 1"
        ),
        [1, 2, 3]
    );
    // Left-fold would have given ({1} ∪ l) ∩ r = {2,3} — pin the
    // difference: 1 must survive.
    // Leading intersect still folds left correctly:
    // (l ∩ r) EXCEPT SELECT 2 = {3}.
    assert_eq!(
        ints(
            &mut e,
            "SELECT x FROM l INTERSECT SELECT x FROM r EXCEPT SELECT 2"
        ),
        [3]
    );
}

#[test]
fn parenthesized_groups_override_precedence() {
    let mut e = Engine::new();
    setup(&mut e);
    // (1 UNION l) INTERSECT r — the explicit group forces the
    // union first: {1,2,3} ∩ {2,3,4} = {2,3} (1 must NOT survive,
    // the mirror of the precedence test above).
    assert_eq!(
        ints(
            &mut e,
            "(SELECT 1 UNION SELECT x FROM l) INTERSECT SELECT x FROM r \
             ORDER BY 1"
        ),
        [2, 3]
    );
    // Peer-position group: l EXCEPT (r EXCEPT SELECT 3)
    // = {1,2,3} - {2,4} = {1,3}.
    assert_eq!(
        ints(
            &mut e,
            "SELECT x FROM l EXCEPT (SELECT x FROM r EXCEPT SELECT 3) \
             ORDER BY 1"
        ),
        [1, 3]
    );
}

#[test]
fn group_internal_order_by_and_limit() {
    let mut e = Engine::new();
    setup(&mut e);
    // The group's LIMIT applies inside the parens: top-2 of r
    // ordered desc = {4, 3}, then UNION with {1} = {1, 3, 4}.
    assert_eq!(
        ints(
            &mut e,
            "(SELECT x FROM r ORDER BY x DESC LIMIT 2) UNION SELECT 1 \
             ORDER BY 1"
        ),
        [1, 3, 4]
    );
    // Peer-position group with its own LIMIT: {1} ∪ top-1 of l
    // ascending = {1} ∪ {1} → dedup {1}.
    assert_eq!(
        ints(
            &mut e,
            "SELECT 1 UNION (SELECT x FROM l ORDER BY x LIMIT 1)"
        ),
        [1]
    );
}

#[test]
fn chains_with_union() {
    let mut e = Engine::new();
    setup(&mut e);
    // Left-associative chain: (l INTERSECT r) UNION ALL SELECT 99.
    assert_eq!(
        ints(
            &mut e,
            "SELECT x FROM l INTERSECT SELECT x FROM r \
             UNION ALL SELECT 99 ORDER BY x"
        ),
        [2, 3, 99]
    );
}
