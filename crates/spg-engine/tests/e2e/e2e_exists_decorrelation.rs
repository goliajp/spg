//! v7.34 (mailrs conn-pool-exhaustion P0) — correlated `[NOT] EXISTS`
//! semi/anti-join decorrelation (`try_batch_correlated_exists`). The
//! reported `count_unseen` ran two correlated `NOT EXISTS` per ~24k join
//! survivors (98.7% of a 1.4 s query); decorrelation builds the inner
//! key-set once and reduces each per-row EXISTS to a membership test.
//!
//! These pin that the rewrite is answer-identical to the per-row
//! resolver across the cases that break a naive implementation:
//! EXISTS vs NOT EXISTS, single- vs multi-column correlation, NULL outer
//! key, NULL inner key, an inner predicate, and an empty match set. The
//! JOIN form routes the WHERE through `filter_join_survivors` — the
//! decorrelation site. The perf speedup itself is pinned by the
//! `perf_gate::count_unseen` probe.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn setup() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE mb (id INT, acc TEXT)").unwrap();
    e.execute("INSERT INTO mb (id, acc) VALUES (1, 'a@x')")
        .unwrap();
    e.execute("CREATE TABLE o (id INT, mailbox_id INT, k TEXT, k2 TEXT)")
        .unwrap();
    e.execute(
        "INSERT INTO o (id, mailbox_id, k, k2) VALUES \
         (1, 1, 't1', 'a@x'), (2, 1, 't2', 'a@x'), (3, 1, 't3', 'a@x'), \
         (4, 1, NULL, 'a@x'), (5, 1, 't5', 'a@x')",
    )
    .unwrap();
    e.execute("CREATE TABLE i (ik TEXT, ik2 TEXT, cat TEXT)")
        .unwrap();
    e.execute(
        "INSERT INTO i (ik, ik2, cat) VALUES \
         ('t1', 'a@x', 'spam'), ('t2', 'a@x', 'general'), ('t3', 'b@x', 'spam'), \
         (NULL, 'a@x', 'spam'), ('t5', 'a@x', 'scam')",
    )
    .unwrap();
    e
}

fn ids(e: &mut Engine, sql: &str) -> Vec<i64> {
    let rows = match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from {sql:?}, got {other:?}"),
    };
    rows.iter()
        .map(|r| match &r.values[0] {
            Value::Int(n) => i64::from(*n),
            Value::BigInt(n) => *n,
            other => panic!("expected int id, got {other:?}"),
        })
        .collect()
}

const JOIN: &str = "FROM o JOIN mb ON o.mailbox_id = mb.id";

#[test]
fn not_exists_single_col_anti_join() {
    let mut e = setup();
    // inner key-set {t1,t2,t3,t5} (NULL dropped). NOT EXISTS keeps rows
    // whose k is absent: only o4 (NULL k → never present → kept).
    let got = ids(
        &mut e,
        &format!(
            "SELECT o.id {JOIN} WHERE NOT EXISTS (SELECT 1 FROM i WHERE i.ik = o.k) ORDER BY o.id"
        ),
    );
    assert_eq!(got, vec![4]);
}

#[test]
fn exists_single_col_semi_join() {
    let mut e = setup();
    // EXISTS keeps the present rows; o4 (NULL) excluded.
    let got = ids(
        &mut e,
        &format!(
            "SELECT o.id {JOIN} WHERE EXISTS (SELECT 1 FROM i WHERE i.ik = o.k) ORDER BY o.id"
        ),
    );
    assert_eq!(got, vec![1, 2, 3, 5]);
}

#[test]
fn not_exists_with_inner_predicate() {
    let mut e = setup();
    // inner-pred cat IN ('spam','scam') → set {t1,t3,t5}. NOT EXISTS keeps
    // o2 (t2 general, not in set) and o4 (NULL).
    let got = ids(
        &mut e,
        &format!(
            "SELECT o.id {JOIN} WHERE NOT EXISTS \
             (SELECT 1 FROM i WHERE i.ik = o.k AND i.cat IN ('spam','scam')) ORDER BY o.id"
        ),
    );
    assert_eq!(got, vec![2, 4]);
}

#[test]
fn not_exists_multi_column_correlation() {
    let mut e = setup();
    // Two correlations (i.ik=o.k AND i.ik2=o.k2). Tuple set
    // {(t1,a@x),(t2,a@x),(t3,b@x),(t5,a@x)}. o3 probes (t3,a@x) — absent
    // (set has (t3,b@x)) → kept; o4 (NULL,_) → kept.
    let got = ids(
        &mut e,
        &format!(
            "SELECT o.id {JOIN} WHERE NOT EXISTS \
             (SELECT 1 FROM i WHERE i.ik = o.k AND i.ik2 = o.k2) ORDER BY o.id"
        ),
    );
    assert_eq!(got, vec![3, 4]);
}

#[test]
fn empty_match_set_keeps_all_not_exists_excludes_all_exists() {
    let mut e = setup();
    // No inner row has cat 'zzz' → empty set.
    let kept = ids(
        &mut e,
        &format!(
            "SELECT o.id {JOIN} WHERE NOT EXISTS \
             (SELECT 1 FROM i WHERE i.ik = o.k AND i.cat = 'zzz') ORDER BY o.id"
        ),
    );
    assert_eq!(kept, vec![1, 2, 3, 4, 5]);
    let none = ids(
        &mut e,
        &format!(
            "SELECT o.id {JOIN} WHERE EXISTS \
             (SELECT 1 FROM i WHERE i.ik = o.k AND i.cat = 'zzz') ORDER BY o.id"
        ),
    );
    assert!(none.is_empty(), "expected no rows, got {none:?}");
}
