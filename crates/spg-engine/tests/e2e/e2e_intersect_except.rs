//! v7.37.17 (17.6 siblings) — INTERSECT [ALL] + EXCEPT [ALL] set
//! operations.

use spg_engine::{Engine, QueryResult};

fn ints(e: &mut Engine, sql: &str) -> Vec<i64> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
    e.execute("INSERT INTO l VALUES (1), (2), (2), (3)").unwrap();
    e.execute("CREATE TABLE r (x INT)").unwrap();
    // Right multiset: {2, 3, 3, 4}.
    e.execute("INSERT INTO r VALUES (2), (3), (3), (4)").unwrap();
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
